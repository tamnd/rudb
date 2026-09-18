//! How many rows a node produces, guessed.
//!
//! `spec/09-optimizer.md` section 9.3 puts cardinality estimation under join ordering, and join
//! ordering is not here yet. This arrives before it because two smaller things need it first: the
//! build side flag on [`Node::Join`], which is a choice between two numbers, and `EXPLAIN`, which
//! has to print something next to each operator. Both of those want the same function and neither
//! of them wants a search.
//!
//! Almost everything here is a guess and the type says so. [`rows`] returns `None` rather than a
//! default, because a caller that has to decide between two sides can only do that when it has two
//! numbers, and a made up number that looks like a measurement is how an optimizer talks itself
//! into the wrong plan. The one rule the whole module follows is that a node whose input is unknown
//! is unknown: uncertainty travels up rather than being rounded away at the first operator that has
//! a formula.
//!
//! [`rows_stat`] is the same walk with the class kept, and it is the one that says which numbers
//! are not guesses. A scan is the catalog's count and is exact, a Parquet read is the sum of the
//! footers the binder read and is exact too, a cross product of two counted sides is arithmetic on
//! counted numbers and is exact, a `LIMIT` over an unknown input is a real ceiling, and everything
//! above the first filter, group by or equijoin is an estimate from a constant. Both functions walk
//! the same tree and the numbers they give back are the same numbers, so a caller that only
//! compares two sides can go on using [`rows`] and ignore all of this.
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

use rudb_common::stat::{Class, Direction, Provenance, Stat};
use rudb_plan::{ConjunctionOp, Expr, ExprRef, JoinKind, Node, NodeRef, Plan, SetOpKind};

use crate::walk;

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
/// Files are not in here. What a Parquet call produces is counted by the binder, which is the only
/// thing in the chain holding the file open, and it rides on the plan against the table index of
/// the call. See [`Plan::measured`].
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

/// The class of a number that came out of one of the constants above.
const GUESSED: Class = Class::Estimated;

/// Where a number that came out of one of the constants above says it came from.
///
/// [`Provenance::Default`] and not [`Provenance::Propagation`], because the guess is the constant
/// and the propagation only carried it. The point of printing the provenance in `EXPLAIN` is to
/// find the place where nobody had a number, and this is that place.
const FROM_A_CONSTANT: Provenance = Provenance::Default;

/// The class of a number that is a proven ceiling with nothing under it.
///
/// A `LIMIT 10` over an unknown input produces somewhere between no rows and ten, so the value is
/// certain from above and the relative error can be the whole of it, which is a bound of one. That
/// is the weakest certificate there is and it is still worth telling apart from a guess: a guess
/// can be exceeded and this cannot.
const CEILING: Class = Class::Certified { bound: 1.0, direction: Direction::AtMost };

/// How many rows this node is guessed to produce, where a guess can be made at all.
///
/// `None` means nothing downstream of here should pretend to know, which is the answer for a scan
/// of a table nobody measured, for a table function nobody measured either, and for anything above
/// either of those.
///
/// The same answer as [`rows_stat`] read for the Decide use of `spec/stats/05-every-query.md`
/// section 5.1.1, which is the only use a cardinality is ever put to: it chooses between two plans
/// that produce the same rows, so every class is allowed through and a caller that gets `None` has
/// to fall back to a documented default rather than to a number. A caller that wants to answer a
/// query from this, or to license a rewrite with it, has to call [`rows_stat`] and ask with
/// [`Stat::answer`] or [`Stat::enable`], and both of those will refuse almost everything this
/// module produces. That is the point.
#[must_use]
pub fn rows(plan: &Plan, node: NodeRef, stats: &Statistics) -> Option<u64> {
    rows_stat(plan, node, stats).decide().copied()
}

/// How many rows this node produces, and how much of that is knowledge.
///
/// `Unknown` means nothing downstream of here should pretend to know, which is the answer for a
/// scan of a table nobody measured, for every table function, and for anything above either of
/// those. A `Known` carries the class of `spec/stats/04-in-memory.md` section 4.1, and the classes
/// combine up the tree the way that document's section 4.7 asks: a number derived from an exact one
/// and a guess is a guess, and the degradation is never rounded away.
///
/// This walks the subtree once per call and does not cache. A caller that wants the whole plan
/// annotated will walk it top down and ask for each node, which is quadratic in the depth, and a
/// plan deep enough for that to matter is a plan with other problems. The cache goes in when join
/// ordering arrives and asks the same question about the same subtree a thousand times.
#[must_use]
pub fn rows_stat(plan: &Plan, node: NodeRef, stats: &Statistics) -> Stat<u64> {
    let of = |child: NodeRef| rows_stat(plan, child, stats);
    match *plan.node(node) {
        // One row with no columns, which is what a `SELECT` with no `FROM` is bound against.
        Node::Dummy => Stat::exact(1, Provenance::RowCount),
        // The catalog counted these rather than estimating them, so the count is the count. That
        // is the one exact number a plan starts from today and it is why the histogram does not
        // read all unknown: a scan knows, and everything above it stops knowing.
        Node::Get { catalog, schema, table, .. } => {
            match stats.rows_in(plan.string(catalog), plan.string(schema), plan.string(table)) {
                Some(rows) => Stat::exact(rows, Provenance::RowCount),
                None => Stat::Unknown,
            }
        }
        // Counted rather than guessed. A literal row list is the one place in a plan where the
        // number of rows is written down.
        Node::Values { rows: list, .. } => u64::try_from(plan.row_list(list).len())
            .map_or(Stat::Unknown, |rows| Stat::exact(rows, Provenance::RowCount)),
        // Whatever the binder measured, which for a Parquet read is the sum of the footers and is
        // exact, and for every other table function is unknown. The answer comes from the binder
        // because the binder is the only thing in the chain with the file open, and it is read from
        // the plan rather than worked out here because a function nobody taught this about has to
        // stay unknown. Guessing on behalf of all of them is the failure mode this module exists to
        // avoid.
        Node::TableFunction { index, .. } => plan.measured(index),
        // Once per row of whatever is on its left, and nothing here knows how many rows that is or
        // how many the call gives back for each of them.
        Node::LateralFunction { .. } => Stat::Unknown,
        Node::Filter { input, predicate } => {
            let kept = KEPT_BY_A_CONDITION.powi(conjuncts(plan, predicate));
            guess(of(input), kept)
        }
        // A projection changes the width and not the height, and a sort changes neither.
        // A fetch reads a column of each row it is handed, so it is as tall as its input too.
        Node::Project { input, .. }
        | Node::Window { input, .. }
        | Node::Sort { input, .. }
        | Node::Fetch { input, .. }
        | Node::TableFetch { input, .. } => of(input),
        Node::Aggregate { input, groups, .. } => {
            // An aggregate with no group keys produces exactly one row, over an empty input as
            // much as over a billion, which is the one case here that is a fact rather than a
            // guess. `empty_result_pullup` stops at this node for the same reason.
            if plan.expr_list(groups).is_empty() {
                return Stat::exact(1, Provenance::RowCount);
            }
            guess(of(input), KEPT_BY_A_GROUP_BY)
        }
        // The same shape as a group by on those columns, because that is what it is.
        Node::Distinct { input, .. } => guess(of(input), KEPT_BY_A_GROUP_BY),
        Node::Limit { input, count, offset } => {
            let input = of(input);
            match count {
                // `OFFSET` with no `LIMIT` takes rows away and cannot add any, and taking a known
                // number of rows off a counted one leaves a counted one.
                None => input.map(|n| n.saturating_sub(offset)),
                // A limit is a ceiling even when the input is unknown, which is the one place in
                // this module where an unknown input still gives an answer. It is an upper bound
                // rather than an estimate, and for the callers here that is the useful direction:
                // a side that cannot produce more than ten rows is the small side whatever feeds
                // it. An input that was counted keeps its class, because the smaller of two known
                // numbers is known.
                Some(count) => match input {
                    Stat::Unknown => {
                        Stat::Known { value: count, class: CEILING, provenance: FROM_A_CONSTANT }
                    }
                    known => known.map(|n| n.saturating_sub(offset).min(count)),
                },
            }
        }
        Node::TopN { input, count, offset, .. } => match of(input) {
            Stat::Unknown => {
                Stat::Known { value: count, class: CEILING, provenance: FROM_A_CONSTANT }
            }
            known => known.map(|n| n.saturating_sub(offset).min(count)),
        },
        Node::Join { left, right, kind, conditions, .. } => {
            join(of(left), of(right), kind, plan.expr_list(conditions).len())
        }
        // The right cardinality is a function of each left row until decorrelation, so treating it
        // as one independently measured input would be a made-up estimate.
        Node::DependentJoin { .. } => Stat::Unknown,
        // Two counted sides multiply to a counted answer. Nothing is guessed here at all.
        Node::CrossProduct { left, right } => of(left).zip(of(right), u64::saturating_mul),
        // Every set operation is bounded above by both sides together, and `UNION ALL` reaches it.
        // The deduplicating ones and `EXCEPT` are somewhere below it and nothing here knows where,
        // so the bound is what they get, and the bound is what their class says they got.
        // Holding rows does not change how many there are, so a materialisation is as tall as the
        // query that reads it and the definition it holds is counted where it is read.
        Node::MaterializedCte { body, .. } => of(body),
        // What a read produces is what the definition produced, and the definition is above this
        // node rather than under it, which is the one place in a plan where that is true. A walk
        // that only sees the subtree cannot reach it, so this says so rather than guessing. Giving
        // a real answer takes the count being recorded when the definition is walked, which is
        // worth doing when something asks a question this would change the answer to.
        Node::CteScan { .. } => Stat::Unknown,
        Node::SetOp { left, right, kind, all, .. } => {
            let total = of(left).zip(of(right), u64::saturating_add);
            match (kind, all) {
                // `UNION ALL` emits both sides and reaches the bound, so two counted sides give a
                // counted answer.
                (SetOpKind::Union, true) => total,
                _ => ceiling(total),
            }
        }
    }
}

/// One of the constant guesses applied to a child's count.
///
/// `Unknown` in, `Unknown` out, which is the rule the whole module follows. Otherwise the child's
/// class combines with [`GUESSED`], so an exact scan under a filter is an estimate and stays one all
/// the way up.
fn guess(input: Stat<u64>, kept: f64) -> Stat<u64> {
    match input {
        Stat::Unknown => Stat::Unknown,
        Stat::Known { value, class, .. } => Stat::Known {
            value: scale(value, kept).max(1),
            class: class.combine(GUESSED),
            provenance: FROM_A_CONSTANT,
        },
    }
}

/// The same number, said as a ceiling rather than as a count.
fn ceiling(stat: Stat<u64>) -> Stat<u64> {
    match stat {
        Stat::Unknown => Stat::Unknown,
        Stat::Known { value, class, provenance } => {
            Stat::Known { value, class: class.combine(CEILING), provenance }
        }
    }
}

/// How many independent conditions a predicate is made of, capped.
///
/// A top level `AND` is the only thing that splits, which is the same split filter pushdown makes
/// and for the same reason: those are the parts that each have to hold. An `OR` is one condition
/// however many branches it has, and something inside a function call is not reached, because
/// `f(a AND b)` is one predicate about whatever `f` does.
///
/// A conjunct with the same value for every row is not counted. The selectivity constant is a guess
/// about a predicate over data, and a condition that does not read the data keeps every row or none
/// of them rather than a fifth of them. Most of those are folded away before this ever sees them,
/// and the one that survives is the fold that was abandoned so that the error still comes from
/// running the query.
///
/// Capped at eight so that a query written by a generator does not compound its way to a factor of
/// a million. Past a handful of conditions the product has stopped meaning anything anyway, and the
/// cap is where it stops pretending to.
fn conjuncts(plan: &Plan, predicate: ExprRef) -> i32 {
    let counted = match *plan.expr(predicate) {
        Expr::Conjunction { op: ConjunctionOp::And, children } => {
            plan.expr_list(children).iter().filter(|&&part| !walk::constant(plan, part)).count()
        }
        _ => usize::from(!walk::constant(plan, predicate)),
    };
    i32::try_from(counted.min(8)).unwrap_or(8)
}

/// The join kinds, each of which is a different question.
fn join(left: Stat<u64>, right: Stat<u64>, kind: JoinKind, conditions: usize) -> Stat<u64> {
    match kind {
        // Left rows, filtered by whether a match exists. Never more than the left side, and the
        // right side's size does not enter into it.
        JoinKind::Semi => guess(left, KEPT_BY_A_CONDITION),
        JoinKind::Anti => guess(left, 1.0 - KEPT_BY_A_CONDITION),
        // At most one right row each, by definition, which makes this a fact about the node rather
        // than a guess about the data.
        JoinKind::Single | JoinKind::Mark => left,
        // The nth with the nth, so the shorter side decides, and it decides exactly.
        JoinKind::Positional => left.zip(right, u64::min),
        _ => {
            let (
                Stat::Known { value: left, class: left_class, provenance: left_from },
                Stat::Known { value: right, class: right_class, provenance: right_from },
            ) = (left, right)
            else {
                return Stat::Unknown;
            };
            let both = left_class.combine(right_class);
            // Two sides that came from different places make a number that came from the
            // arithmetic, which is what a reader chasing this node needs to be told.
            let from = if left_from == right_from { left_from } else { Provenance::Propagation };
            // A join with no condition is a cross product wearing a different node.
            if conditions == 0 {
                return Stat::Known {
                    value: left.saturating_mul(right),
                    class: both,
                    provenance: from,
                };
            }
            // The containment assumption: every row of the smaller side finds a match, so an
            // equijoin produces about as many rows as its larger side. It is the standard guess and
            // it is right whenever one side of the condition is a key, which is most joins anybody
            // writes and none of the joins that hurt. A many to many join on a low cardinality
            // column produces far more than this, and finding that out needs distinct counts.
            let matched = left.max(right);
            let value = match kind {
                // An outer join emits every row of the preserved side whether it matched or not,
                // so the estimate cannot fall below that side.
                JoinKind::Left => matched.max(left),
                JoinKind::Right => matched.max(right),
                JoinKind::Full => matched.max(left).max(right),
                _ => matched,
            };
            // The containment assumption is the guess, so this is one however exact both sides
            // were. Two counted tables joined on a column nobody has a distinct count for is the
            // single most common way a plan goes wrong, and a class saying exact here would hide
            // exactly that.
            Stat::Known { value, class: both.combine(GUESSED), provenance: FROM_A_CONSTANT }
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
    use rudb_common::stat::{Class, Direction, Provenance, Stat};
    use rudb_plan::Plan;

    use super::{Statistics, rows, rows_stat};

    /// A one column scan of the named table, which is what most of these sit on.
    fn scan(table: &str, index: u32) -> String {
        format!("Get memory.main.{table} AS {table} #{index} [a::INTEGER]\n")
    }

    /// The tables named here, sized as given, and nothing else measured.
    fn statistics(tables: &[(&str, u64)]) -> Statistics {
        let mut stats = Statistics::new();
        for (table, count) in tables {
            stats.record("memory", "main", table, *count);
        }
        stats
    }

    /// The estimate for the root of a plan written as text, against the given table sizes.
    fn estimate(text: &str, tables: &[(&str, u64)]) -> Option<u64> {
        let stats = statistics(tables);
        let plan =
            Plan::parse(text).unwrap_or_else(|error| panic!("{text} did not parse: {error}"));
        rows(&plan, plan.root(), &stats)
    }

    /// The same estimate with the class still attached.
    fn stat(text: &str, tables: &[(&str, u64)]) -> Stat<u64> {
        let stats = statistics(tables);
        let plan =
            Plan::parse(text).unwrap_or_else(|error| panic!("{text} did not parse: {error}"));
        rows_stat(&plan, plan.root(), &stats)
    }

    /// The guess this module has always made, spelled out.
    const GUESSED: Class = Class::Estimated;

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
    fn a_condition_that_reads_no_column_is_not_counted_as_a_condition() {
        // A fifth is a guess about a predicate over data. One that does not read the data keeps
        // every row or none of them, and taking a fifth for it is taking a fifth for nothing.
        let both = format!(
            "Filter ((#0.0::INTEGER > 1::INTEGER)::BOOLEAN AND (1::INTEGER > 2::INTEGER)::BOOLEAN)::BOOLEAN\n  {}",
            scan("t", 0)
        );
        assert_eq!(estimate(&both, &[("t", 1_000_000)]), Some(200_000));
        let alone = format!("Filter (1::INTEGER > 2::INTEGER)::BOOLEAN\n  {}", scan("t", 0));
        assert_eq!(estimate(&alone, &[("t", 1_000_000)]), Some(1_000_000));
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
    fn a_count_that_came_from_the_catalog_says_it_is_exact() {
        // The one number in here that was counted rather than guessed, and the only reason the
        // histogram does not read all unknown at this point in the series.
        assert_eq!(stat(&scan("t", 0), &[("t", 5000)]).class(), Some(Class::Exact));
        assert_eq!(stat(&scan("t", 0), &[]).class(), None);
    }

    #[test]
    fn every_number_here_says_where_it_came_from() {
        // The scan is the catalog's count and says so. The filter over it is the constant and says
        // that, which is the word somebody searches an `EXPLAIN` for when a plan went wrong,
        // because it means nobody had a number at that node at all.
        assert_eq!(stat(&scan("t", 0), &[("t", 5000)]).provenance(), Some(Provenance::RowCount));
        let text = format!("Filter (#0.0::INTEGER > 1::INTEGER)::BOOLEAN\n  {}", scan("t", 0));
        assert_eq!(stat(&text, &[("t", 1000)]).provenance(), Some(Provenance::Default));
    }

    #[test]
    fn a_cardinality_is_for_deciding_and_answers_nothing() {
        // A filtered count is a guess, so the build side chooser is welcome to it and nothing that
        // changes an answer is. This is the rule of section 5.1.1 read off one node, and it is the
        // one a later pass is most likely to break in good faith.
        let text = format!("Filter (#0.0::INTEGER > 1::INTEGER)::BOOLEAN\n  {}", scan("t", 0));
        let guessed = stat(&text, &[("t", 1000)]);
        assert_eq!(guessed.decide(), Some(&200));
        assert_eq!(guessed.answer(), None);
        assert_eq!(guessed.enable(), None);
        // A scan is the one node in here that could answer, and it still only does so because the
        // catalog counted rather than because the walk was clever.
        let counted = stat(&scan("t", 0), &[("t", 5000)]);
        assert_eq!(counted.answer(), Some(&5000));
        assert_eq!(counted.enable(), Some(&5000));
        // Nobody measured the table, so every use gets nothing rather than a zero.
        let nothing = stat(&scan("t", 0), &[]);
        assert_eq!(nothing.decide(), None);
        assert_eq!(nothing.answer(), None);
        assert_eq!(nothing.enable(), None);
    }

    #[test]
    fn one_guess_anywhere_under_a_node_makes_the_node_a_guess() {
        // Exact combined with a guess is the guess. A filter over a counted table is not a counted
        // number any more, and reading the class back as exact is what would make somebody fold a
        // constant on it later.
        let text = format!("Filter (#0.0::INTEGER > 1::INTEGER)::BOOLEAN\n  {}", scan("t", 0));
        assert_eq!(stat(&text, &[("t", 1000)]).class(), Some(GUESSED));
        // And the guess stays the same guess however many of them stack up, since they all come
        // from the same constant.
        let twice = format!(
            "Aggregate #1 groups=[#0.0::INTEGER] aggregates=[]\n  Filter (#0.0::INTEGER > \
             1::INTEGER)::BOOLEAN\n    {}",
            scan("t", 0)
        );
        assert_eq!(stat(&twice, &[("t", 1000)]).class(), Some(GUESSED));
    }

    #[test]
    fn an_ungrouped_aggregate_is_exact_because_one_row_is_a_fact() {
        let text = format!("Aggregate #1 groups=[] aggregates=[]\n  {}", scan("t", 0));
        assert_eq!(stat(&text, &[]).class(), Some(Class::Exact));
    }

    #[test]
    fn a_limit_over_an_unmeasured_input_is_certified_rather_than_estimated() {
        // Ten is not a guess about what the scan produces, it is the most this node can emit, so a
        // caller asking whether the number can be exceeded gets the right answer.
        let text = format!("Limit 10 offset 0\n  {}", scan("t", 0));
        assert_eq!(
            stat(&text, &[]).class(),
            Some(Class::Certified { bound: 1.0, direction: Direction::AtMost })
        );
        // Over a counted input the count wins and the answer is a fact again.
        assert_eq!(stat(&text, &[("t", 3)]).class(), Some(Class::Exact));
    }

    #[test]
    fn a_join_with_no_condition_is_a_product_and_the_product_is_exact() {
        // Every row against every row is arithmetic rather than an assumption. The equijoin next to
        // it is the assumption, and the two should not read the same.
        let product = format!("Join INNER on=[]\n  {}  {}", scan("a", 0), scan("b", 1));
        assert_eq!(stat(&product, &[("a", 1000), ("b", 1000)]).class(), Some(Class::Exact));
        let equi = format!(
            "Join INNER on=[(#0.0::INTEGER = #1.0::INTEGER)::BOOLEAN]\n  {}  {}",
            scan("a", 0),
            scan("b", 1)
        );
        assert_eq!(stat(&equi, &[("a", 1000), ("b", 1000)]).class(), Some(GUESSED));
    }

    #[test]
    fn a_table_function_nobody_measured_is_unknown_and_stays_unknown_over_it() {
        let text = "TableFunction range args=[] #0 [a::BIGINT]\n";
        assert_eq!(stat(text, &[]), Stat::Unknown);
    }

    #[test]
    fn a_table_function_the_binder_measured_is_as_tall_as_the_binder_said() {
        // What a Parquet read looks like from here. The number is the footer's and this module has
        // no opinion about it beyond passing it on with the class it arrived with.
        let text = "TableFunction read_parquet args=[] #0 [a::BIGINT]\n";
        let mut plan = Plan::parse(text).expect("a table function");
        plan.measure(0, Stat::exact(6_001_215, Provenance::RowCount));
        assert_eq!(
            rows_stat(&plan, plan.root(), &Statistics::new()),
            Stat::exact(6_001_215, Provenance::RowCount)
        );
    }

    #[test]
    fn a_table_function_is_measured_against_its_index_and_not_against_its_name() {
        // Two reads of two different files in one statement. The index is what tells them apart,
        // and it is the one identifier a rewrite cannot move without rewriting every expression
        // above it, which is why the measurement is filed under it.
        let text = concat!(
            "Join INNER on=[]\n",
            "  TableFunction read_parquet args=[] #0 [a::BIGINT]\n",
            "  TableFunction read_parquet args=[] #1 [b::BIGINT]\n"
        );
        let mut plan = Plan::parse(text).expect("two table functions");
        plan.measure(0, Stat::exact(3, Provenance::RowCount));
        plan.measure(1, Stat::exact(5, Provenance::RowCount));
        assert_eq!(
            rows_stat(&plan, plan.root(), &Statistics::new()),
            Stat::exact(15, Provenance::RowCount)
        );
    }

    #[test]
    fn a_table_function_nobody_measured_is_unknown_even_beside_one_that_was() {
        // A CSV read says nothing about its own length, so it stays unknown while the Parquet read
        // next to it is counted, and the join over the two is unknown because one side is.
        let text = concat!(
            "Join INNER on=[]\n",
            "  TableFunction read_parquet args=[] #0 [a::BIGINT]\n",
            "  TableFunction read_csv args=[] #1 [b::BIGINT]\n"
        );
        let mut plan = Plan::parse(text).expect("two table functions");
        plan.measure(0, Stat::exact(3, Provenance::RowCount));
        assert_eq!(rows_stat(&plan, plan.root(), &Statistics::new()), Stat::Unknown);
    }

    #[test]
    fn a_guess_over_a_measured_file_is_a_guess_with_a_number_under_it() {
        // The point of measuring the file. Before it the filter had nothing to multiply and came
        // out unknown, and the two constants in this module were dead code on every query over a
        // Parquet file, which is every ClickBench query.
        let text = concat!(
            "Filter (#0.0::BIGINT > 5::BIGINT)::BOOLEAN\n",
            "  TableFunction read_parquet args=[] #0 [a::BIGINT]\n"
        );
        let mut plan = Plan::parse(text).expect("a filter over a table function");
        assert_eq!(rows_stat(&plan, plan.root(), &Statistics::new()), Stat::Unknown);
        plan.measure(0, Stat::exact(1000, Provenance::RowCount));
        let over = rows_stat(&plan, plan.root(), &Statistics::new());
        assert_eq!(over.value().copied(), Some(200));
        assert_eq!(over.class(), Some(Class::Estimated));
        assert_eq!(over.provenance(), Some(Provenance::Default));
    }

    #[test]
    fn a_lateral_function_is_unknown_however_well_the_file_beside_it_is_measured() {
        // It runs once per row of its input and nothing here knows how many rows it gives back for
        // each of them, so a measurement of some other table says nothing about this one.
        let text = concat!(
            "LateralFunction range args=[#0.0::INTEGER] #1 [a::BIGINT]\n",
            "  Get memory.main.t AS t #0 [a::INTEGER]\n"
        );
        let mut plan = Plan::parse(text).expect("a lateral function");
        plan.measure(1, Stat::exact(4096, Provenance::RowCount));
        assert_eq!(rows_stat(&plan, plan.root(), &Statistics::new()), Stat::Unknown);
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
