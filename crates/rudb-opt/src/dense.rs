//! When a grouped aggregate's key is an integer column with no gaps worth speaking of, so the slot
//! a row belongs in can be read rather than looked for.
//!
//! A hash aggregate answers one question per row, which is which group this row's key belongs to.
//! The hash table answers it by hashing the key, masking the hash to a bucket, loading the bucket,
//! comparing a salt, and then loading the stored key to be sure. That is two trips to memory per
//! row and a comparison, and on a table larger than the cache both trips are misses.
//!
//! A key that is one integer column with a known smallest and largest value does not need any of
//! it. The value itself is the address: subtract the smallest and you have a place in an array, and
//! the array is as long as the range. One load, no hashing, no salt, no key comparison, and no
//! collisions at all. `spec/stats/05-every-query.md` section 5.4 calls it a hash table replaced by
//! an array, and the statistic it needs is the pair of ends every store already writes per part.
//!
//! # The range has to be a range the values are inside of
//!
//! Not the range they fill. A column of the numbers one to a thousand with nine hundred of them
//! missing is still direct addressed here, and the ninety percent of the array nobody uses is the
//! price. What is not allowed is a range narrower than the column, because the array would have no
//! place for the value outside it, and the operator would be reading a slot that belongs to some
//! other group. That is a wrong answer, so the pair of ends is read through `extremes::span`,
//! which takes nothing but the two bounds a store really looked at.
//!
//! # Why the density test is not a density test
//!
//! What decides it is the size of the array and not how full it is going to be, because the array
//! is charged against the query's memory budget and how full it gets is a guess. `WIDEST` is the
//! whole of the rule.
//!
//! There is a second test and it is about the guess rather than about the memory. Where the column
//! has a counted distinct value and the range is more than `SPARSEST` times it, the array would
//! be mostly holes, and an array of mostly holes is worse than the hash table it replaces: it
//! touches more cache lines per row than a hash table sized to the groups does. So a counted column
//! that says the range is a bad description of it is left alone, and a column nobody counted is
//! decided by `WIDEST` on its own.
//!
//! # What this does not do
//!
//! It does not take the hash table away. The operator keeps it, fills it beside the array, and uses
//! the array as a shortcut to the slot. The array is the index and the hash table is the proof, and
//! the cost of keeping both is one bucket write per group, which happens once per group and never
//! per row. What it buys is that no reading of this pass, and no bound a store ever writes, can
//! turn into a wrong answer: a value the array has no place for is looked up the way it always was.
//!
//! It is one key column and not several. Two integer columns have a product of ranges and the
//! product is past `WIDEST` almost immediately, so the case that would pay is the case where both
//! ranges are tiny, and that is a case where the hash table is already in the first level of cache.
//!
//! It is not a key that is an expression. `GROUP BY c / 100` has a range that is a hundredth of
//! `c`'s and reading through the expression to say so is a separate piece of work.

use rudb_common::Result;
use rudb_common::rules::Rule;
use rudb_plan::{Expr, Node, Plan};

use crate::estimate::{self, DISTINCT, Facts};
use crate::extremes;
use crate::pass::{Context, Pass, top_down};

/// The widest range this will take room for.
///
/// A million and a bit values at four bytes each, which is four megabytes of array per aggregate
/// instance before a row has arrived. One per instance and so the number that matters is this one
/// times the thread count, which is the reasoning behind the size: an aggregate that took room it
/// never fills is charged for it, and a query that used to run and now reports being out of memory
/// is worse than a query that is slow.
///
/// Per instance and not per radix partition, which the operator arranges. A partition is split by
/// hash bits and any value can land in any of them, so a partition's array would have to cover the
/// whole range anyway and there are sixty four of them to an instance. The partitions probe the
/// buckets, which is what they did before this existed.
const WIDEST: u64 = 1 << 20;

/// How many times the counted distinct values a range is allowed to be.
///
/// Eight. An array eight times the groups is eight times the cache lines a hash table sized to the
/// groups would touch, walking the same number of rows, and past about that the array stops being
/// an optimization and starts being a way to miss the cache in a straight line. Below it the array
/// wins on every row, because a hit is one load and the hash table's is two.
const SPARSEST: u64 = 8;

/// Records the range of every grouped aggregate whose key is one integer column with known ends.
///
/// A rudb name rather than a DuckDB one, because DuckDB has no pass that does this and
/// [`crate::UPSTREAM`] is the list of names it does have.
///
/// Two settings turn it off and they mean different things. `SET disabled_optimizers =
/// 'aggregate_dense'` is the pass, which is the door DuckDB's name for a pass goes through. `SET
/// stats_direct_addressing = 'off'` is [`Rule::DirectAddressing`], which is the rule, and that is the
/// door `spec/stats/09-measurement.md` section 9.2 asks for so that a report can say what this rule
/// on its own earned. `SET statistics = 'off'` is the master over the second of them.
#[derive(Debug)]
pub struct AggregateDense;

impl Pass for AggregateDense {
    fn name(&self) -> &'static str {
        "aggregate_dense"
    }

    fn run(&self, plan: &mut Plan, context: &Context) -> Result<()> {
        if context.allows(Rule::DirectAddressing) {
            densify(plan, context.facts());
        }
        Ok(())
    }
}

/// Records a range for every aggregate in `plan` that has one worth acting on.
///
/// Idempotent, because the answer is a function of the plan's shape and the bounds the stores
/// already wrote, and this pass changes neither.
fn densify(plan: &mut Plan, stats: &Facts) {
    let mut found = Vec::new();
    for node in top_down(plan) {
        let Node::Aggregate { index, groups, .. } = *plan.node(node) else {
            continue;
        };
        // One column and one only, for the reason in the module doc.
        let &[key] = plan.expr_list(groups) else { continue };
        let &Expr::Column(binding) = plan.expr(key) else { continue };
        let Some(values) = range(plan, binding, stats) else { continue };
        found.push((index, values));
    }
    for (index, (low, values)) in found {
        plan.densify(index, low, values);
    }
}

/// The smallest value the key column can hold and how many values its range covers.
///
/// `None` where there is no range, where the range is wider than `WIDEST`, and where a counted
/// distinct value says the range describes the column badly.
fn range(plan: &Plan, binding: rudb_plan::ColumnBinding, stats: &Facts) -> Option<(i128, u64)> {
    let (low, high) = extremes::span(plan, binding, 16)?;
    // The count is inclusive of both ends and cannot overflow the subtraction, because both came
    // out of an `i128` and the range of one of those fits in a `u128`.
    let values = u64::try_from(high.checked_sub(low)?.checked_add(1)?).ok()?;
    if values > WIDEST {
        return None;
    }
    // A counted column gets the second test. An uncounted one does not, since there is nothing to
    // compare the range against and the size test has already been passed.
    if let Some(&distinct) = estimate::stated(plan, binding, stats).read(DISTINCT) {
        if distinct > 0 && values > distinct.saturating_mul(SPARSEST) {
            return None;
        }
    }
    Some((low, values))
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use rudb_common::Stat;
    use rudb_common::bounds::{Bound, End, Spread, Test, Zones};
    use rudb_common::stat::Provenance;
    use rudb_plan::Plan;

    use super::{AggregateDense, Rule, WIDEST};
    use crate::estimate::Facts;
    use crate::pass::{Context, Pass};

    /// One table of one column called `a`, named the way the printer names one.
    const SCAN: &str = "Get memory.main.t AS t #0 [a::INTEGER]";

    /// A store of one column called `a` whose ends are whatever a test says.
    #[derive(Debug)]
    struct Stub {
        low: Stat<Bound>,
        high: Stat<Bound>,
    }

    impl Stub {
        fn exact(low: i128, high: i128) -> Arc<Self> {
            Arc::new(Self {
                low: Stat::exact(Bound::Int(low), Provenance::ZoneMap),
                high: Stat::exact(Bound::Int(high), Provenance::ZoneMap),
            })
        }

        /// A store whose ends are both unknown, which is a column nobody bounded.
        fn silent() -> Arc<Self> {
            Arc::new(Self { low: Stat::Unknown, high: Stat::Unknown })
        }
    }

    impl Zones for Stub {
        fn column(&self, name: &str) -> Option<usize> {
            (name == "a").then_some(0)
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

        fn nulls(&self, _column: usize) -> Stat<u64> {
            Stat::Unknown
        }
    }

    /// A plan over one table with one column, grouped on that column, with those ends on it.
    fn grouped(zones: Arc<Stub>) -> Plan {
        let text = format!("Aggregate #1 groups=[#0.0::INTEGER] aggregates=[]\n  {SCAN}\n");
        let mut plan = Plan::parse(&text).expect("a plan that parses");
        plan.set_zones(0, zones as Arc<dyn Zones>);
        plan
    }

    /// A set of facts with a distinct count for the one column, or none at all.
    fn counted(distinct: Option<u64>) -> Context {
        let mut facts = Facts::new();
        facts.record("memory", "main", "t", 1_000_000);
        if let Some(distinct) = distinct {
            facts.record_distinct("memory", "main", "t", "a", distinct, Provenance::Dictionary);
        }
        let mut context = Context::new();
        context.measure(Arc::new(facts));
        context
    }

    fn run(plan: &mut Plan, context: &Context) {
        AggregateDense.run(plan, context).expect("a pass that cannot fail");
    }

    /// The same facts with one rule turned off, which is what an ablation run does.
    fn without(rule: Rule) -> Context {
        let mut context = counted(Some(100));
        let mut rules = rudb_common::rules::Rules::default();
        rules.set(rule, false);
        context.govern(rules);
        context
    }

    #[test]
    fn the_rule_s_own_setting_turns_it_off() {
        let mut plan = grouped(Stub::exact(100, 199));
        run(&mut plan, &without(Rule::DirectAddressing));
        assert_eq!(plan.dense_count(), 0, "stats_direct_addressing = off left the hash table");
    }

    #[test]
    fn the_master_setting_turns_it_off_too() {
        // `statistics = off` reaches this without naming it, which is the point of a master: the
        // ablation of section 9.3 is one statement and it has to cover a rule written after it.
        let mut plan = grouped(Stub::exact(100, 199));
        run(&mut plan, &without(Rule::StatsAll));
        assert_eq!(plan.dense_count(), 0, "statistics = off left the hash table");
    }

    #[test]
    fn a_bounded_key_column_becomes_a_range() {
        let mut plan = grouped(Stub::exact(100, 199));
        run(&mut plan, &counted(Some(100)));
        assert_eq!(plan.dense(1), Some((100, 100)));
    }

    #[test]
    fn a_negative_low_end_is_kept_as_it_is() {
        // The low end is subtracted and not compared against zero, so a column of temperatures is
        // the same case as a column of counts.
        let mut plan = grouped(Stub::exact(-40, 59));
        run(&mut plan, &counted(Some(100)));
        assert_eq!(plan.dense(1), Some((-40, 100)));
    }

    #[test]
    fn one_value_is_one_cell() {
        let mut plan = grouped(Stub::exact(7, 7));
        run(&mut plan, &counted(Some(1)));
        assert_eq!(plan.dense(1), Some((7, 1)));
    }

    #[test]
    fn a_column_nobody_bounded_is_left_alone() {
        let mut plan = grouped(Stub::silent());
        run(&mut plan, &counted(Some(100)));
        assert_eq!(plan.dense_count(), 0);
    }

    #[test]
    fn a_range_past_the_widest_is_left_alone() {
        let wide = i128::from(WIDEST) + 1;
        let mut plan = grouped(Stub::exact(0, wide));
        run(&mut plan, &counted(None));
        assert_eq!(plan.dense_count(), 0);
    }

    #[test]
    fn a_range_that_is_mostly_holes_is_left_alone() {
        // A thousand values wide and ten of them used, which is a hundred times the count.
        let mut plan = grouped(Stub::exact(0, 999));
        run(&mut plan, &counted(Some(10)));
        assert_eq!(plan.dense_count(), 0);
    }

    #[test]
    fn an_uncounted_column_is_decided_by_the_size_alone() {
        let mut plan = grouped(Stub::exact(0, 999));
        run(&mut plan, &counted(None));
        assert_eq!(plan.dense(1), Some((0, 1000)));
    }

    #[test]
    fn an_ungrouped_aggregate_has_no_key_to_address() {
        let text = format!("Aggregate #1 groups=[] aggregates=[count_star()::BIGINT]\n  {SCAN}\n");
        let mut plan = Plan::parse(&text).expect("a plan that parses");
        plan.set_zones(0, Stub::exact(0, 99) as Arc<dyn Zones>);
        run(&mut plan, &counted(None));
        assert_eq!(plan.dense_count(), 0);
    }

    #[test]
    fn a_second_run_writes_what_the_first_one_wrote() {
        let mut plan = grouped(Stub::exact(100, 199));
        let context = counted(Some(100));
        run(&mut plan, &context);
        let once = plan.dense(1);
        run(&mut plan, &context);
        assert_eq!(plan.dense(1), once);
        assert_eq!(plan.dense_count(), 1);
    }

    #[test]
    fn the_pass_is_off_when_it_is_named() {
        let context = Context::without("aggregate_dense").expect("a name that is a pass");
        assert!(context.is_disabled("aggregate_dense"));
    }
}
