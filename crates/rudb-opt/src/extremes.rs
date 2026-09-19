//! Answering `MIN` and `MAX` out of the zone maps instead of off the column.
//!
//! A Parquet writer records the smallest and the largest value of every column of every row group in
//! the footer. `SELECT MIN(EventDate), MAX(EventDate) FROM hits` is asking for the smallest of those
//! smallests and the largest of those largests, and the footer has already been read by the time
//! anything plans the query. So the query is a fold over a few thousand numbers the planner is
//! holding rather than a hundred million values off disk, and the plan it turns into is a row of
//! constants with no scan under it at all.
//!
//! This is the first thing in the engine that reads a statistic to answer with. Everything else
//! reads one to decide between two plans that produce the same rows, where a wrong number is a slow
//! query. Here a wrong number is a wrong answer, which is why the whole path is built around the
//! class rather than around the value: [`Zones::extreme`] hands back a [`Stat`], the pass reads it
//! through [`EXTREME`], and the answer rule of `spec/stats/05-every-query.md` section 5.1.1 lets
//! nothing but an exact one through.
//!
//! # What makes a bound exact
//!
//! A writer is allowed to shorten a long string bound as long as it moves it outward, so a shortened
//! minimum is no larger than the smallest value and a shortened maximum is no smaller than the
//! largest. That keeps every skip the pruner makes correct and it is exactly what stops the bound
//! from being an answer: `MIN(URL)` off a shortened minimum is a string that is not in the column.
//! Parquet has two flags for this, `is_min_value_exact` and `is_max_value_exact`, and most writers
//! state neither.
//!
//! Where nothing is stated the physical type decides it, because shortening is defined for byte
//! strings and for nothing else. A number has no prefix to keep and a writer that dropped half of an
//! `INT64` would not have a bound at all. So an integer, a date and a timestamp are exact by
//! construction and the two byte array types are not exact unless somebody said so.
//!
//! That rule is what makes this work where DuckDB v1.5.5 gives up. A file DuckDB wrote states both
//! flags on every chunk and DuckDB folds `MIN` and `MAX` over it into a constant. The ClickBench
//! `hits.parquet` states neither flag on any of its twenty three thousand chunks, and DuckDB plans a
//! full scan for q7 while this answers it from the footer.
//!
//! # Nulls
//!
//! `MIN` and `MAX` skip nulls, and so does the format: a chunk of nothing but nulls has no bound in
//! it. So a chunk that states no bound but does state that all of its values are null is skipped
//! here as well, and a column with nulls scattered through it still answers. A file where every
//! chunk is null has no bound anywhere, and the answer to `MIN` over it is `NULL`, which this does
//! not produce. It answers nothing and the scan runs, which is a slow right answer.
//!
//! # What is deliberately not here
//!
//! `COUNT(*)`, which is a row count rather than a bound and is already exact on the scan. Any
//! aggregate that is not `MIN` or `MAX`, because no other one is a value the bounds hold. A `FILTER
//! (WHERE ...)` on the aggregate, and anything at all between the aggregate and the scan that is not
//! a projection of column references, because the bounds are the whole column's and a filter under
//! the aggregate means the query asked about part of it.
//!
//! [`Stat`]: rudb_common::Stat
//! [`Zones::extreme`]: rudb_common::bounds::Zones::extreme

use rudb_common::bounds::End;
use rudb_common::stat::Use;
use rudb_common::{Field, LogicalType, Result, Value};
use rudb_plan::{Expr, Node, NodeRef, Plan};

use crate::pass::{Context, Pass};

/// What a zone map bound is read for when it stands in for the query's answer.
///
/// One constant rather than an `.answer()` written out at the call site, for the same reason
/// `crate::estimate::CARDINALITY` is one: the word `EXPLAIN` prints and the rule the number went
/// through are the same constant, so they cannot drift apart.
///
/// [`Use::Answer`] and nothing else. The number produced here is printed to the person who asked, so
/// a bound that is off by a byte is a wrong answer and not a slow query, and the class rule refuses
/// everything that is not exact.
pub const EXTREME: Use = Use::Answer;

/// Folds an ungrouped `MIN` or `MAX` over a whole column into the constant the bounds already know.
#[derive(Debug, Clone, Copy)]
pub struct StatisticsPropagation;

impl Pass for StatisticsPropagation {
    fn name(&self) -> &'static str {
        "statistics_propagation"
    }

    fn run(&self, plan: &mut Plan, _context: &Context) -> Result<()> {
        fold(plan);
        Ok(())
    }
}

/// Replaces every ungrouped aggregate the bounds can answer with the row it answers.
pub fn fold(plan: &mut Plan) {
    for at in 0..u32::try_from(plan.node_count()).unwrap_or(u32::MAX) {
        if let Some((index, fields, values)) = answered(plan, at) {
            let row: Vec<_> = fields
                .iter()
                .zip(values)
                .map(|(field, value)| {
                    let value = plan.add_value(value);
                    plan.add_expr(Expr::Constant(value), field.ty.clone())
                })
                .collect();
            let columns = plan.add_fields(&fields);
            let row = plan.add_expr_list(&row);
            let rows = plan.add_rows(&[row]);
            // Written over the aggregate's own slot rather than appended, so whatever pointed at it
            // keeps pointing at the right thing. That is allowed because the replacement is a leaf,
            // and the arena only asks that a node's children sit behind it.
            *plan.node_mut(at) = Node::Values { index, columns, rows };
        }
    }
}

/// The one row this node produces, where the bounds under it can produce it.
///
/// `None` for every node that is not an ungrouped aggregate of nothing but `MIN` and `MAX` over
/// columns of one scan whose store kept bounds, and for every one of those where any bound is not an
/// exact value. There is no partial answer: a `MIN` this can fold beside a `SUM` it cannot has to
/// keep the scan for the sum, and folding one of the two would mean scanning the column twice.
fn answered(plan: &Plan, at: NodeRef) -> Option<(u32, Vec<Field>, Vec<Value>)> {
    let Node::Aggregate { input, index, groups, aggregates } = *plan.node(at) else {
        return None;
    };
    if !plan.expr_list(groups).is_empty() {
        return None;
    }
    let aggregates = plan.expr_list(aggregates).to_vec();
    if aggregates.is_empty() {
        return None;
    }
    let mut fields = Vec::with_capacity(aggregates.len());
    let mut values = Vec::with_capacity(aggregates.len());
    for aggregate in aggregates {
        let Expr::Aggregate { name, args, filter, .. } = *plan.expr(aggregate) else {
            return None;
        };
        // DISTINCT is not consulted. The smallest of a set is the smallest of the list it came
        // from, which is the one modifier `MIN` and `MAX` do not care about.
        if filter.is_some() {
            return None;
        }
        let end = match plan.string(name) {
            "min" => End::Low,
            "max" => End::High,
            _ => return None,
        };
        let &[arg] = plan.expr_list(args) else { return None };
        let &Expr::Column(binding) = plan.expr(arg) else { return None };
        let ty = plan.expr_type(aggregate).clone();
        let value = extreme(plan, input, binding.table, binding.column as usize, end, &ty)?;
        fields.push(Field::new(plan.string(name), ty));
        values.push(value);
    }
    Some((index, fields, values))
}

/// The value at `end` of the column that binding names, as the type the aggregate produces.
///
/// The binding is followed down the input chain rather than looked up by its table index, because
/// the question is about the rows this aggregate sees and not about the column the index names. A
/// filter between the two would be invisible to a lookup and it is the whole difference between the
/// column's smallest value and the smallest value of the part of it the query asked about.
fn extreme(
    plan: &Plan,
    input: NodeRef,
    table: u32,
    column: usize,
    end: End,
    ty: &LogicalType,
) -> Option<Value> {
    let (index, name) = scanned(plan, input, table, column, 16)?;
    let zones = plan.zones(index)?;
    let stat = zones.extreme(zones.column(&name)?, end);
    stat.read(EXTREME)?.into_value(ty)
}

/// The scan's table index and the name that column has in it, walking down from `at`.
///
/// Only a projection of column references is walked through, and the binding has to name the node it
/// is standing on, so a chain that runs through anything else stops here. The name rather than the
/// position is what comes back, because column pruning moves a scan's positions and moves nothing
/// else, and the store numbers its columns the way the file does.
fn scanned(
    plan: &Plan,
    at: NodeRef,
    table: u32,
    column: usize,
    depth: u32,
) -> Option<(u32, String)> {
    let depth = depth.checked_sub(1)?;
    match *plan.node(at) {
        Node::Get { index, columns, .. } | Node::TableFunction { index, columns, .. }
            if index == table =>
        {
            Some((index, plan.field_list(columns).get(column)?.name.clone()))
        }
        Node::Project { index, input, exprs, .. } if index == table => {
            let &carried = plan.expr_list(exprs).get(column)?;
            let &Expr::Column(binding) = plan.expr(carried) else { return None };
            scanned(plan, input, binding.table, binding.column as usize, depth)
        }
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use rudb_common::Stat;
    use rudb_common::bounds::{Bound, End, Spread, Test, Zones};
    use rudb_common::stat::Provenance;
    use rudb_plan::Plan;

    use super::fold;

    /// A store of one column called `d` whose bounds are whatever a test says.
    #[derive(Debug)]
    struct Stub {
        low: Stat<Bound>,
        high: Stat<Bound>,
    }

    impl Stub {
        /// A store whose bounds are both exact, which is the case the pass answers from.
        fn exact(low: i128, high: i128) -> Arc<Self> {
            Arc::new(Self {
                low: Stat::exact(Bound::Int(low), Provenance::ZoneMap),
                high: Stat::exact(Bound::Int(high), Provenance::ZoneMap),
            })
        }
    }

    impl Zones for Stub {
        fn column(&self, name: &str) -> Option<usize> {
            (name == "d").then_some(0)
        }

        fn surviving(&self, _tests: &[Test]) -> Option<u64> {
            None
        }

        fn spread(&self, _tests: &[Test]) -> Option<Spread> {
            None
        }

        fn extreme(&self, _column: usize, end: End) -> Stat<Bound> {
            match end {
                End::Low => self.low.clone(),
                End::High => self.high.clone(),
            }
        }
    }

    /// What the plan a text prints looks like once the pass has run over a scan with those bounds.
    fn folded(text: &str, zones: &Arc<Stub>) -> String {
        let mut plan =
            Plan::parse(text).unwrap_or_else(|error| panic!("{text} did not parse: {error}"));
        plan.set_zones(0, Arc::clone(zones) as Arc<dyn Zones>);
        fold(&mut plan);
        plan.validate().unwrap_or_else(|error| panic!("{text} did not stay valid: {error}"));
        plan.to_string()
    }

    #[test]
    fn a_minimum_and_a_maximum_over_a_whole_column_become_the_two_numbers_the_bounds_hold() {
        let text = "Aggregate #1 groups=[] aggregates=[min(#0.0::INTEGER)::INTEGER, \
                    max(#0.0::INTEGER)::INTEGER]\n  \
                    Get memory.main.t AS t #0 [d::INTEGER]\n";
        assert_eq!(
            folded(text, &Stub::exact(3, 91)),
            "Values #1 [min::INTEGER, max::INTEGER] rows=[[3::INTEGER, 91::INTEGER]]\n"
        );
    }

    #[test]
    fn a_projection_between_the_two_is_followed_through_and_the_renamed_column_still_answers() {
        let text = "Aggregate #2 groups=[] aggregates=[min(#1.0::INTEGER)::INTEGER]\n  \
                    Project #1 [#0.0::INTEGER AS renamed]\n    \
                    Get memory.main.t AS t #0 [d::INTEGER]\n";
        assert_eq!(
            folded(text, &Stub::exact(3, 91)),
            "Values #2 [min::INTEGER] rows=[[3::INTEGER]]\n"
        );
    }

    #[test]
    fn a_filter_between_the_two_stops_it_because_the_bounds_are_the_whole_columns() {
        let text = "Aggregate #1 groups=[] aggregates=[min(#0.0::INTEGER)::INTEGER]\n  \
                    Filter (#0.0::INTEGER > 50::INTEGER)::BOOLEAN\n    \
                    Get memory.main.t AS t #0 [d::INTEGER]\n";
        assert_eq!(folded(text, &Stub::exact(3, 91)), text);
    }

    #[test]
    fn a_bound_the_writer_widened_is_not_an_answer_however_good_a_bound_it_is() {
        let text = "Aggregate #1 groups=[] aggregates=[min(#0.0::INTEGER)::INTEGER]\n  \
                    Get memory.main.t AS t #0 [d::INTEGER]\n";
        let widened = Arc::new(Stub { low: Stat::Unknown, high: Stat::Unknown });
        assert_eq!(folded(text, &widened), text);
    }

    #[test]
    fn one_aggregate_the_bounds_cannot_answer_keeps_the_scan_for_all_of_them() {
        let text = "Aggregate #1 groups=[] aggregates=[min(#0.0::INTEGER)::INTEGER, \
                    sum(#0.0::INTEGER)::HUGEINT]\n  \
                    Get memory.main.t AS t #0 [d::INTEGER]\n";
        assert_eq!(folded(text, &Stub::exact(3, 91)), text);
    }

    #[test]
    fn a_group_by_is_not_this_even_when_every_aggregate_in_it_is_a_minimum() {
        let text = "Aggregate #1 groups=[#0.0::INTEGER] aggregates=[min(#0.0::INTEGER)::INTEGER]\n  \
                    Get memory.main.t AS t #0 [d::INTEGER]\n";
        assert_eq!(folded(text, &Stub::exact(3, 91)), text);
    }

    #[test]
    fn a_table_that_kept_no_bounds_at_all_is_left_to_the_scan() {
        let text = "Aggregate #1 groups=[] aggregates=[min(#0.0::INTEGER)::INTEGER]\n  \
                    Get memory.main.t AS t #0 [d::INTEGER]\n";
        let mut plan =
            Plan::parse(text).unwrap_or_else(|error| panic!("{text} did not parse: {error}"));
        fold(&mut plan);
        assert_eq!(plan.to_string(), text);
    }
}
