//! How large a grouped aggregate's hash table should be before its first row arrives.
//!
//! A hash aggregate starts with sixty four buckets and doubles. Every doubling walks the groups it
//! already holds and writes each one into a new bucket array, so a group by that ends with a
//! million groups has written about two million buckets on the way to holding one million, and the
//! last few doublings are the ones that move nearly all of it. None of that work produces a row.
//! `spec/stats/05-every-query.md` section 5.4 says the number that removes it is the distinct count
//! of the grouping key, and that the count is exact for the keys this matters most on.
//!
//! This pass writes one number per aggregate and moves nothing. A plan it has run over produces the
//! rows it produced before, in the order it produced them, because the operator it is talking to
//! reads the number as a size and never as a count of anything.
//!
//! # The number is a ceiling and not an estimate
//!
//! Two facts bound how many groups an aggregate can make, and both point the same way.
//!
//! A grouping key that is one column of one table cannot make more groups than that column has
//! distinct values. Two key columns cannot make more than the product of theirs. Neither is affected
//! by anything between the scan and the aggregate, because a filter removes rows and removing rows
//! never adds a group.
//!
//! The rows arriving are the other bound, because a group needs a row in it.
//!
//! The smaller of two ceilings is a ceiling, which is what this takes. What it deliberately does not
//! take is [`estimate::rows`] of the aggregate itself, which is the modelled group count: that
//! number is a guess about how the keys landed, it is [`rudb_common::Class::Estimated`] wherever it
//! comes from, and sizing an allocation from a guess is the mistake section 5.1 names.
//!
//! # Why a ceiling is the safe end of the range here
//!
//! Section 5.1 says to pick the end of the range whose failure you can afford, and the two failures
//! are not symmetric. A table sized under the truth grows the way it grew before this pass existed,
//! which costs the rehashing this pass was written to remove and nothing else. A table sized over
//! the truth holds bucket memory it never fills, and bucket memory is charged against the query's
//! budget, so a large enough overshoot turns a query that ran into a query that reports being out of
//! memory. That is worse than a slow query, and it is why `MOST` exists.
//!
//! # What it leaves alone
//!
//! An ungrouped aggregate, which produces exactly one row and has no table.
//!
//! A key that is not columns. `GROUP BY lower(c)` has as many groups as `c` has values at most, and
//! reading through the expression to say so is a separate piece of work that is not this one.
//!
//! A key column nobody counted, which is a column of a table with no dictionary, no sketch and no
//! footer entry. There is no ceiling to take and the honest answer is the size the table always
//! started at.
//!
//! A ceiling that is already inside the first bucket array, because a table that was never going to
//! grow has nothing to save.

use rudb_common::{Class, Direction, Result, Stat};
use rudb_plan::{Node, Plan};

use crate::estimate::{self, DISTINCT, Facts};
use crate::pass::{Context, Pass, top_down};

/// The most groups this pass will ask for room for.
///
/// Sixteen million buckets at eight bytes each, which is the hundred and twenty eight megabytes an
/// aggregate instance would be holding before it had folded a row in. Every instance of a
/// partitioned aggregate would hold its own, so the number that matters is this one times the thread
/// count, and that is the reasoning behind it being a ceiling rather than a size: an overshoot here
/// is charged against the query's memory budget and can fail a query that used to run. Beyond this
/// the doubling can have the rest, which by then is the last two or three of them.
const MOST: u64 = 8 << 20;

/// The most groups a table that has not grown yet can hold.
///
/// `rudb_exec`'s table starts at sixty four buckets and grows when it is half full, so it holds
/// thirty two groups before the first doubling. A ceiling at or under this is a table that was never
/// going to grow, and asking for the size it already has would put an entry in the plan that changes
/// nothing.
const ALREADY: u64 = 32;

/// Writes the group count onto every aggregate that has a ceiling worth acting on.
///
/// A rudb name rather than a DuckDB one, because DuckDB has no pass that does this and [`crate::UPSTREAM`]
/// is the list of names it does have. `SET disabled_optimizers = 'aggregate_presize'` is the setting
/// `spec/stats/09-measurement.md` section 9.3's ablation turns this off with.
#[derive(Debug)]
pub struct AggregatePresize;

impl Pass for AggregatePresize {
    fn name(&self) -> &'static str {
        "aggregate_presize"
    }

    fn run(&self, plan: &mut Plan, context: &Context) -> Result<()> {
        size(plan, context.facts());
        Ok(())
    }
}

/// Records a size for every aggregate in `plan` that has one.
///
/// Idempotent, because the answer is a function of the plan's shape and the facts, and this pass
/// changes neither. Running it twice writes the same entries over the same entries.
fn size(plan: &mut Plan, stats: &Facts) {
    let mut found = Vec::new();
    for node in top_down(plan) {
        let Node::Aggregate { input, index, groups, .. } = *plan.node(node) else {
            continue;
        };
        let keys = plan.expr_list(groups);
        if keys.is_empty() {
            continue;
        }
        let Some(ceiling) = ceiling(plan, input, keys, stats) else {
            continue;
        };
        if ceiling <= ALREADY {
            continue;
        }
        found.push((index, ceiling.min(MOST)));
    }
    for (index, ceiling) in found {
        plan.presize(index, ceiling);
    }
}

/// The most groups the aggregate over `input` keyed on `keys` could possibly produce.
///
/// `None` when nothing bounds it, which is the ordinary answer for a key that is an expression and
/// for a column of a table nobody counted.
fn ceiling(
    plan: &Plan,
    input: rudb_plan::NodeRef,
    keys: &[rudb_plan::ExprRef],
    stats: &Facts,
) -> Option<u64> {
    let bindings = estimate::keyed(plan, keys)?;
    let mut values: u64 = 1;
    for binding in bindings {
        values = values.saturating_mul(bounded(estimate::stated(plan, binding, stats))?);
    }
    // A group needs a row in it, so the rows arriving bound the groups too. Unknown leaves the key
    // ceiling standing on its own, which is still a ceiling.
    Some(match estimate::rows(plan, input, stats) {
        Some(rows) => values.min(rows),
        None => values,
    })
}

/// The distinct count read as a ceiling, or `None` when it is not one.
///
/// [`Class::Exact`] is a ceiling because it is the number. [`Direction::AtMost`] is a ceiling by what
/// the direction means, whatever the bound is. The other two directions are not ceilings at all, and
/// they are taken anyway when the bound is inside a factor of two, because the table doubles: a size
/// wrong by less than that is one doubling from right in whichever direction it is wrong, and one
/// doubling is what this pass is trying to save fifteen of.
///
/// [`Class::Estimated`] is refused. The number a guess would size the allocation from is the thing
/// section 5.1 says not to allocate from.
fn bounded(stat: Stat<u64>) -> Option<u64> {
    let value = *stat.read(DISTINCT)?;
    match stat.class()? {
        Class::Exact => Some(value),
        Class::Certified { direction: Direction::AtMost, .. } => Some(value),
        Class::Certified { bound, .. } if bound <= 1.0 => Some(value),
        Class::Certified { .. } | Class::Estimated => None,
    }
}

#[cfg(test)]
mod tests {
    use rudb_common::Provenance;
    use rudb_plan::Plan;

    use super::{AggregatePresize, MOST};
    use crate::estimate::Facts;
    use crate::pass::{Context, Pass};

    /// One table of one column, named the way the printer names one.
    const SCAN: &str = "Get memory.main.t AS t #0 [a::INTEGER]";

    /// A plan over one table with one column, grouped on that column.
    fn grouped(filter: Option<&str>) -> Plan {
        let scan = SCAN;
        let text = match filter {
            None => format!("Aggregate #1 groups=[#0.0::INTEGER] aggregates=[]\n  {scan}\n"),
            Some(predicate) => format!(
                "Aggregate #1 groups=[#0.0::INTEGER] aggregates=[]\n  Filter {predicate}\n    \
                 {scan}\n"
            ),
        };
        Plan::parse(&text).expect("a plan that parses")
    }

    fn counted(rows: u64, distinct: u64) -> Context {
        let mut facts = Facts::new();
        facts.record("memory", "main", "t", rows);
        facts.record_distinct("memory", "main", "t", "a", distinct, Provenance::Dictionary);
        let mut context = Context::new();
        context.measure(std::sync::Arc::new(facts));
        context
    }

    fn run(plan: &mut Plan, context: &Context) {
        AggregatePresize.run(plan, context).expect("a pass that cannot fail");
    }

    #[test]
    fn a_counted_key_column_sizes_the_table() {
        let mut plan = grouped(None);
        run(&mut plan, &counted(1_000_000, 50_000));
        assert_eq!(plan.presized(1), Some(50_000));
    }

    #[test]
    fn the_rows_arriving_are_the_other_ceiling() {
        // More distinct values than there are rows, which a fold across parts can say. The rows
        // are the smaller ceiling and the smaller of two ceilings is the one to take.
        let mut plan = grouped(None);
        run(&mut plan, &counted(4_000, 50_000));
        assert_eq!(plan.presized(1), Some(4_000));
    }

    #[test]
    fn a_filter_underneath_does_not_raise_the_ceiling() {
        // The filter's own estimate is below the row count and the key ceiling is unchanged by it,
        // because removing rows never adds a group. Whichever is smaller, the answer is a ceiling.
        let mut plan = grouped(Some("(#0.0::INTEGER > 10::INTEGER)::BOOLEAN"));
        run(&mut plan, &counted(1_000_000, 50_000));
        let sized = plan.presized(1).expect("an aggregate with a ceiling");
        assert!(sized <= 50_000, "{sized} is above the key's own ceiling");
    }

    #[test]
    fn a_column_nobody_counted_is_left_alone() {
        let mut plan = grouped(None);
        let mut facts = Facts::new();
        facts.record("memory", "main", "t", 1_000_000);
        let mut context = Context::new();
        context.measure(std::sync::Arc::new(facts));
        run(&mut plan, &context);
        assert_eq!(plan.presized(1), None);
        assert_eq!(plan.presized_count(), 0);
    }

    #[test]
    fn an_ungrouped_aggregate_has_no_table_to_size() {
        let text = format!("Aggregate #1 groups=[] aggregates=[count_star()::BIGINT]\n  {SCAN}\n");
        let mut plan = Plan::parse(&text).expect("a plan that parses");
        run(&mut plan, &counted(1_000_000, 50_000));
        assert_eq!(plan.presized_count(), 0);
    }

    #[test]
    fn a_key_that_never_fills_the_first_buckets_is_left_alone() {
        let mut plan = grouped(None);
        run(&mut plan, &counted(1_000_000, 8));
        assert_eq!(plan.presized_count(), 0);
    }

    #[test]
    fn a_ceiling_past_the_cap_is_capped() {
        let mut plan = grouped(None);
        run(&mut plan, &counted(u64::MAX, u64::MAX));
        assert_eq!(plan.presized(1), Some(MOST));
    }

    #[test]
    fn a_second_run_writes_what_the_first_one_wrote() {
        let mut plan = grouped(None);
        let context = counted(1_000_000, 50_000);
        run(&mut plan, &context);
        let once = plan.presized(1);
        run(&mut plan, &context);
        assert_eq!(plan.presized(1), once);
        assert_eq!(plan.presized_count(), 1);
    }

    #[test]
    fn the_pass_is_off_when_it_is_named() {
        let context = Context::without("aggregate_presize").expect("a name that is a pass");
        assert!(context.is_disabled("aggregate_presize"));
    }
}
