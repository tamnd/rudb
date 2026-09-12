//! How many rows a node produces, guessed.
//!
//! `spec/09-optimizer.md` section 9.3 puts cardinality estimation under join ordering, and join
//! ordering is not here yet. This arrives before it because two smaller things need it first: the
//! build side flag on [`Node::Join`], which is a choice between two numbers, and `EXPLAIN`, which
//! has to print something next to each operator. Both of those want the same function and neither
//! of them wants a search.
//!
//! Everything here is a guess and the type says so. [`rows`] returns `None` rather than a default,
//! because a caller that has to decide between two sides can only do that when it has two numbers,
//! and a made up number that looks like a measurement is how an optimizer talks itself into the
//! wrong plan. The one rule the whole module follows is that a node whose input is unknown is
//! unknown: uncertainty travels up rather than being rounded away at the first operator that has a
//! formula.
//!
//! The constants are the textbook ones, which is to say they are DuckDB's, which is to say they
//! are Selinger's. They are wrong for any particular query and they are wrong in a direction that
//! does not depend on the query, which is the property that makes them usable: two sides of a join
//! estimated the same wrong way still compare correctly most of the time, and comparing is all the
//! first caller does. Nothing here should be read as a row count. It is a way of ordering two
//! plans.
//!
//! What is missing is every part of estimation that needs data rather than shape. There are no
//! column histograms, no distinct counts, no correlation between predicates, and no sample. A
//! filter on a primary key and a filter on a boolean get the same selectivity here. That is the
//! part `spec/09-optimizer.md` section 9.3 actually specifies and it needs the statistics that
//! M3's storage layer collects, so it waits for them.

use std::collections::BTreeMap;

use rudb_plan::{ConjunctionOp, Expr, ExprRef, JoinKind, Node, NodeRef, Plan};

/// What one conjunct of a filter is assumed to keep.
///
/// A fifth, which is DuckDB's default for a predicate it cannot reason about and has been the
/// textbook guess since System R. It is too generous for an equality on a key and far too harsh
/// for `WHERE x > 0` on a column of counts, and it is applied per conjunct, so three conditions
/// anded together take a table to one row in a hundred and twenty five. That compounding is the
/// part most likely to be wrong, and it is kept because the alternative is to treat a query with
/// three conditions as though it were as selective as a query with one.
const KEPT_BY_A_CONDITION: f64 = 0.2;

/// What a group by is assumed to collapse its input to.
///
/// A tenth. Grouping is the operator where shape alone says the least: `GROUP BY user_id` over a
/// log table is close to one row in one, and `GROUP BY country` over the same table is a few
/// hundred rows out of any number. Without distinct counts there is nothing to tell them apart, so
/// this is a middle that is wrong for both rather than a guess that favours one.
const KEPT_BY_A_GROUP_BY: f64 = 0.1;

/// The row counts the optimizer was handed, by table.
///
/// A side table rather than a field on [`Node::Get`], and a plain count rather than a handle on the
/// catalog. Both of those are so that a plan stays a value: the optimizer's own tests build plans
/// out of text with no database anywhere near them, `Plan::parse` of a printed plan gives back the
/// plan it was printed from, and neither of those survives a node that carries a number only a
/// live catalog could have filled in.
///
/// Empty is the ordinary state for anything that is not a real query, and an empty one makes every
/// scan unknown rather than making every scan zero. A scan of a table nobody measured and a scan of
/// an empty table are not the same thing, and an optimizer that confuses them will happily build a
/// hash table from the side it thinks has no rows.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Statistics {
    tables: BTreeMap<(String, String, String), u64>,
}

impl Statistics {
    /// Nothing known about anything.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Record what one table held.
    pub fn record(&mut self, catalog: &str, schema: &str, table: &str, rows: u64) {
        self.tables.insert((catalog.to_owned(), schema.to_owned(), table.to_owned()), rows);
    }

    /// What that table held, where anybody said.
    #[must_use]
    pub fn rows_in(&self, catalog: &str, schema: &str, table: &str) -> Option<u64> {
        self.tables.get(&(catalog.to_owned(), schema.to_owned(), table.to_owned())).copied()
    }

    /// Whether anything at all was recorded.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.tables.is_empty()
    }
}

/// How many rows this node is guessed to produce, where a guess can be made at all.
///
/// `None` means nothing downstream of here should pretend to know, which is the answer for a scan
/// of a table nobody measured, for every table function, and for anything above either of those.
///
/// This walks the subtree once per call and does not cache. A caller that wants the whole plan
/// annotated will walk it top down and ask for each node, which is quadratic in the depth, and a
/// plan deep enough for that to matter is a plan with other problems. The cache goes in when join
/// ordering arrives and asks the same question about the same subtree a thousand times.
#[must_use]
pub fn rows(plan: &Plan, node: NodeRef, stats: &Statistics) -> Option<u64> {
    let of = |child: NodeRef| rows(plan, child, stats);
    match *plan.node(node) {
        // One row with no columns, which is what a `SELECT` with no `FROM` is bound against.
        Node::Dummy => Some(1),
        Node::Get { catalog, schema, table, .. } => {
            stats.rows_in(plan.string(catalog), plan.string(schema), plan.string(table))
        }
        // Counted rather than guessed. A literal row list is the one place in a plan where the
        // number of rows is written down.
        Node::Values { rows: list, .. } => u64::try_from(plan.row_list(list).len()).ok(),
        // A table function is an open door. `read_parquet` could answer this from the footer and
        // one day should, but the answer would have to come from the reader rather than from here,
        // and a function nobody taught this about would still be unknown. Guessing on behalf of all
        // of them is the failure mode this module exists to avoid.
        Node::TableFunction { .. } => None,
        Node::Filter { input, predicate } => {
            let kept = KEPT_BY_A_CONDITION.powi(conjuncts(plan, predicate));
            of(input).map(|n| scale(n, kept).max(1))
        }
        // A projection changes the width and not the height, and a sort changes neither.
        Node::Project { input, .. } | Node::Sort { input, .. } => of(input),
        Node::Aggregate { input, groups, .. } => {
            // An aggregate with no group keys produces exactly one row, over an empty input as
            // much as over a billion, which is the one case here that is a fact rather than a
            // guess. `empty_result_pullup` stops at this node for the same reason.
            if plan.expr_list(groups).is_empty() {
                return Some(1);
            }
            of(input).map(|n| scale(n, KEPT_BY_A_GROUP_BY).max(1))
        }
        // The same shape as a group by on those columns, because that is what it is.
        Node::Distinct { input, .. } => of(input).map(|n| scale(n, KEPT_BY_A_GROUP_BY).max(1)),
        Node::Limit { input, count, offset } => {
            let input = of(input);
            match count {
                // `OFFSET` with no `LIMIT` takes rows away and cannot add any.
                None => input.map(|n| n.saturating_sub(offset)),
                // A limit is a ceiling even when the input is unknown, which is the one place in
                // this module where an unknown input still gives an answer. It is an upper bound
                // rather than an estimate, and for the callers here that is the useful direction:
                // a side that cannot produce more than ten rows is the small side whatever feeds
                // it.
                Some(count) => Some(input.map_or(count, |n| n.saturating_sub(offset).min(count))),
            }
        }
        Node::TopN { input, count, offset, .. } => {
            Some(of(input).map_or(count, |n| n.saturating_sub(offset).min(count)))
        }
        Node::Join { left, right, kind, conditions } => {
            join(of(left), of(right), kind, plan.expr_list(conditions).len())
        }
        Node::CrossProduct { left, right } => match (of(left), of(right)) {
            (Some(left), Some(right)) => Some(left.saturating_mul(right)),
            _ => None,
        },
        // Every set operation is bounded above by both sides together, and `UNION ALL` reaches it.
        // The deduplicating ones and `EXCEPT` are somewhere below it and nothing here knows where,
        // so the bound is what they get.
        Node::SetOp { left, right, .. } => match (of(left), of(right)) {
            (Some(left), Some(right)) => Some(left.saturating_add(right)),
            _ => None,
        },
    }
}

/// How many independent conditions a predicate is made of, capped.
///
/// A top level `AND` is the only thing that splits, which is the same split filter pushdown makes
/// and for the same reason: those are the parts that each have to hold. An `OR` is one condition
/// however many branches it has, and something inside a function call is not reached, because
/// `f(a AND b)` is one predicate about whatever `f` does.
///
/// Capped at eight so that a query written by a generator does not compound its way to a factor of
/// a million. Past a handful of conditions the product has stopped meaning anything anyway, and the
/// cap is where it stops pretending to.
fn conjuncts(plan: &Plan, predicate: ExprRef) -> i32 {
    let counted = match *plan.expr(predicate) {
        Expr::Conjunction { op: ConjunctionOp::And, children } => plan.expr_list(children).len(),
        _ => 1,
    };
    i32::try_from(counted.min(8)).unwrap_or(8)
}

/// The eight join kinds, each of which is a different question.
fn join(left: Option<u64>, right: Option<u64>, kind: JoinKind, conditions: usize) -> Option<u64> {
    match kind {
        // Left rows, filtered by whether a match exists. Never more than the left side, and the
        // right side's size does not enter into it.
        JoinKind::Semi => left.map(|n| scale(n, KEPT_BY_A_CONDITION).max(1)),
        JoinKind::Anti => left.map(|n| scale(n, 1.0 - KEPT_BY_A_CONDITION).max(1)),
        // At most one right row each, by definition.
        JoinKind::Single => left,
        // The nth with the nth, so the shorter side decides.
        JoinKind::Positional => match (left, right) {
            (Some(left), Some(right)) => Some(left.min(right)),
            _ => None,
        },
        _ => {
            let (Some(left), Some(right)) = (left, right) else { return None };
            // A join with no condition is a cross product wearing a different node.
            if conditions == 0 {
                return Some(left.saturating_mul(right));
            }
            // The containment assumption: every row of the smaller side finds a match, so an
            // equijoin produces about as many rows as its larger side. It is the standard guess and
            // it is right whenever one side of the condition is a key, which is most joins anybody
            // writes and none of the joins that hurt. A many to many join on a low cardinality
            // column produces far more than this, and finding that out needs distinct counts.
            let matched = left.max(right);
            Some(match kind {
                // An outer join emits every row of the preserved side whether it matched or not,
                // so the estimate cannot fall below that side.
                JoinKind::Left => matched.max(left),
                JoinKind::Right => matched.max(right),
                JoinKind::Full => matched.max(left).max(right),
                _ => matched,
            })
        }
    }
}

/// A row count times a fraction, without letting the float arithmetic invent anything.
#[expect(
    clippy::cast_precision_loss,
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss,
    reason = "an estimate going through f64 is the point, and the result is clamped"
)]
fn scale(rows: u64, by: f64) -> u64 {
    let scaled = rows as f64 * by;
    if scaled.is_finite() && scaled >= 0.0 { scaled.min(u64::MAX as f64) as u64 } else { 0 }
}

#[cfg(test)]
mod tests {
    use rudb_plan::Plan;

    use super::{Statistics, rows};

    /// A one column scan of the named table, which is what most of these sit on.
    fn scan(table: &str, index: u32) -> String {
        format!("Get memory.main.{table} AS {table} #{index} [a::INTEGER]\n")
    }

    /// The estimate for the root of a plan written as text, against the given table sizes.
    fn estimate(text: &str, tables: &[(&str, u64)]) -> Option<u64> {
        let mut stats = Statistics::new();
        for (table, count) in tables {
            stats.record("memory", "main", table, *count);
        }
        let plan =
            Plan::parse(text).unwrap_or_else(|error| panic!("{text} did not parse: {error}"));
        rows(&plan, plan.root(), &stats)
    }

    #[test]
    fn a_scan_is_what_the_catalog_said_and_nothing_when_nobody_said() {
        let text = scan("t", 0);
        assert_eq!(estimate(&text, &[("t", 5000)]), Some(5000));
        // Not zero. A table nobody measured and an empty table are different, and an optimizer
        // that confuses them builds its hash table from the wrong side.
        assert_eq!(estimate(&text, &[]), None);
        assert_eq!(estimate(&text, &[("t", 0)]), Some(0));
    }

    #[test]
    fn not_knowing_travels_up_rather_than_being_rounded_away() {
        // The filter has a formula and the thing under it does not, so the filter has no answer
        // either. This is the property the whole module rests on.
        let text = format!("Filter (#0.0::INTEGER > 1::INTEGER)::BOOLEAN\n  {}", scan("t", 0));
        assert_eq!(estimate(&text, &[]), None);
        assert!(estimate(&text, &[("t", 1000)]).is_some());
    }

    #[test]
    fn an_ungrouped_aggregate_is_one_row_whatever_is_under_it() {
        // The one answer here that is a fact rather than a guess, and it holds with no statistics
        // at all, which is what makes it worth special casing.
        let text = format!("Aggregate #1 groups=[] aggregates=[]\n  {}", scan("t", 0));
        assert_eq!(estimate(&text, &[]), Some(1));
        assert_eq!(estimate(&text, &[("t", 9_000_000)]), Some(1));
    }

    #[test]
    fn a_group_by_collapses_its_input_and_a_scan_under_it_still_decides_whether_it_can() {
        let text = format!("Aggregate #1 groups=[#0.0::INTEGER] aggregates=[]\n  {}", scan("t", 0));
        assert_eq!(estimate(&text, &[("t", 1000)]), Some(100));
        assert_eq!(estimate(&text, &[]), None);
    }

    #[test]
    fn a_limit_is_a_ceiling_even_over_an_input_nobody_measured() {
        // The only place an unknown input still produces a number. It is an upper bound rather
        // than an estimate, and a side that cannot produce more than ten rows is the small side of
        // a join whatever feeds it.
        let text = format!("Limit 10 offset 0\n  {}", scan("t", 0));
        assert_eq!(estimate(&text, &[]), Some(10));
        assert_eq!(estimate(&text, &[("t", 3)]), Some(3));
        assert_eq!(estimate(&text, &[("t", 3_000_000)]), Some(10));
    }

    #[test]
    fn an_offset_with_no_limit_takes_rows_away_and_cannot_add_any() {
        let text = format!("Limit ALL offset 5\n  {}", scan("t", 0));
        assert_eq!(estimate(&text, &[("t", 12)]), Some(7));
        assert_eq!(estimate(&text, &[("t", 2)]), Some(0));
        // No ceiling to fall back on here, so an unmeasured input stays unmeasured.
        assert_eq!(estimate(&text, &[]), None);
    }

    #[test]
    fn a_filter_never_estimates_a_relation_away_entirely() {
        // Six conditions at a fifth each is a factor of fifteen thousand, and a plan that believes
        // a subtree produces no rows is a plan that stops reading it. Pruning a subtree is
        // `empty_result_pullup`'s job and it does it from a proof rather than from a guess.
        let and = "(#0.0::INTEGER > 1::INTEGER)::BOOLEAN AND (#0.0::INTEGER > 2::INTEGER)::BOOLEAN \
                   AND (#0.0::INTEGER > 3::INTEGER)::BOOLEAN AND \
                   (#0.0::INTEGER > 4::INTEGER)::BOOLEAN AND (#0.0::INTEGER > 5::INTEGER)::BOOLEAN \
                   AND (#0.0::INTEGER > 6::INTEGER)::BOOLEAN";
        let text = format!("Filter ({and})::BOOLEAN\n  {}", scan("t", 0));
        assert_eq!(estimate(&text, &[("t", 10)]), Some(1));
        // And each part counts, so six of them cut harder than one of them.
        let one = format!("Filter (#0.0::INTEGER > 1::INTEGER)::BOOLEAN\n  {}", scan("t", 0));
        assert_eq!(estimate(&one, &[("t", 1_000_000)]), Some(200_000));
    }

    #[test]
    fn an_inner_join_comes_out_the_size_of_its_larger_side() {
        // The containment assumption. Ten thousand against fifty thousand is fifty thousand.
        let text = format!(
            "Join INNER on=[(#0.0::INTEGER = #1.0::INTEGER)::BOOLEAN]\n  {}  {}",
            scan("small", 0),
            scan("big", 1)
        );
        assert_eq!(estimate(&text, &[("small", 10_000), ("big", 50_000)]), Some(50_000));
        // And one side missing is the whole join missing, since the assumption is about both.
        assert_eq!(estimate(&text, &[("small", 10_000)]), None);
    }

    #[test]
    fn a_join_with_no_condition_is_the_product_and_says_so() {
        let text = format!("Join INNER on=[]\n  {}  {}", scan("small", 0), scan("big", 1));
        assert_eq!(estimate(&text, &[("small", 1000), ("big", 1000)]), Some(1_000_000));
    }

    #[test]
    fn an_outer_join_never_estimates_below_the_side_it_preserves() {
        // A left join that keeps every left row cannot produce fewer than that, however small the
        // right side is, and the containment assumption on its own would say otherwise.
        let text = format!(
            "Join LEFT on=[(#0.0::INTEGER = #1.0::INTEGER)::BOOLEAN]\n  {}  {}",
            scan("big", 0),
            scan("small", 1)
        );
        assert_eq!(estimate(&text, &[("big", 50_000), ("small", 10)]), Some(50_000));
    }

    #[test]
    fn a_semi_join_is_bounded_by_its_left_side_and_ignores_the_right() {
        // A semi join emits left rows, once each. However large the right side is, it cannot make
        // more of them, and the nested loop that runs it today should still know which side is
        // which.
        let text = format!(
            "Join SEMI on=[(#0.0::INTEGER = #1.0::INTEGER)::BOOLEAN]\n  {}  {}",
            scan("small", 0),
            scan("big", 1)
        );
        let estimated =
            estimate(&text, &[("small", 1000), ("big", 9_000_000)]).expect("both sides known");
        assert!(estimated <= 1000, "a semi join produced {estimated} out of 1000 left rows");
    }

    #[test]
    fn a_cross_product_of_two_enormous_sides_saturates_rather_than_wrapping() {
        // The number is meaningless and the point is that it is enormous rather than that it is
        // small, which is what a wrap would turn it into.
        let text = format!("CrossProduct\n  {}  {}", scan("a", 0), scan("b", 1));
        assert_eq!(estimate(&text, &[("a", u64::MAX), ("b", 2)]), Some(u64::MAX));
    }

    #[test]
    fn a_union_all_is_both_sides_and_so_is_the_bound_on_the_rest_of_them() {
        let text = format!("SetOp UNION ALL #2\n  {}  {}", scan("a", 0), scan("b", 1));
        assert_eq!(estimate(&text, &[("a", 30), ("b", 12)]), Some(42));
    }

    #[test]
    fn statistics_that_nobody_filled_in_say_so() {
        let mut stats = Statistics::new();
        assert!(stats.is_empty());
        stats.record("memory", "main", "t", 7);
        assert!(!stats.is_empty());
        assert_eq!(stats.rows_in("memory", "main", "t"), Some(7));
        // The three names are one key. A table of the same name in another schema is another table.
        assert_eq!(stats.rows_in("memory", "other", "t"), None);
    }
}
