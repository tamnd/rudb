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

use rudb_common::bounds::Test;
use rudb_common::stat::{Class, Direction, Provenance, Stat};
use rudb_plan::{
    ColumnBinding, CompareOp, ConjunctionOp, Expr, ExprRef, JoinKind, Node, NodeRef, Plan,
    SetOpKind, Slice,
};

use crate::{bounds, walk};

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
/// the call. See [`Plan::measured`] and [`Plan::distinct_measured`].
///
/// The distinct counts are here beside the row counts and not somewhere else, because the two are
/// asked together: `join` divides one by the other and a divisor that arrived by a different road
/// than the dividend is a divisor nobody can keep in step.
///
/// Empty is the ordinary state for anything that is not a real query, and an empty one makes every
/// scan unknown rather than making every scan zero. A scan of a table nobody measured and a scan of
/// an empty table are not the same thing, and an optimizer that confuses them will happily build a
/// hash table from the side it thinks has no rows.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Statistics {
    tables: BTreeMap<(String, String, String), u64>,
    columns: BTreeMap<(String, String, String, String), u64>,
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

    /// Record how many distinct values one column of one table holds.
    ///
    /// By name and not by position, because the position a column has in a scan is whatever is left
    /// after column pruning moved it and the name is not moved by anything.
    pub fn record_distinct(
        &mut self,
        catalog: &str,
        schema: &str,
        table: &str,
        column: &str,
        distinct: u64,
    ) {
        let key = (catalog.to_owned(), schema.to_owned(), table.to_owned(), column.to_owned());
        self.columns.insert(key, distinct);
    }

    /// How many distinct values that column holds, where anybody counted.
    #[must_use]
    pub fn distinct_in(
        &self,
        catalog: &str,
        schema: &str,
        table: &str,
        column: &str,
    ) -> Option<u64> {
        let key = (catalog.to_owned(), schema.to_owned(), table.to_owned(), column.to_owned());
        self.columns.get(&key).copied()
    }

    /// Whether anything at all was recorded.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.tables.is_empty() && self.columns.is_empty()
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
            let (kept, from) = kept(plan, predicate, stats);
            // The bounds are a ceiling over the guess and not a new thing to guess about. A store
            // that keeps a minimum and a maximum per part can say which parts this filter rules
            // out, and the rows in the parts that survive is a number no answer to this filter can
            // exceed. So the guess runs exactly as it always did and the ceiling is applied to it,
            // which is the one composition that cannot be worse than the guess alone: the ceiling
            // is never below the truth, so it replaces the guess only where the guess was above
            // the truth and it lands no further from it than the guess was.
            //
            // Taking the ceiling as the estimate instead, or taking a fifth of it, both measured
            // better on the fourteen predicates in the pull request that added this and neither is
            // safe in general. A fifth of a ceiling that is already tight is a number below the
            // truth, which is the direction that picks the wrong build side, and a ceiling read as
            // an estimate is above the guess on any predicate the bounds barely narrow.
            match surviving(plan, input, predicate) {
                // Provably none. Every part is ruled out by bounds that cannot be wrong in this
                // direction, so this is a fact and not an estimate, and it is the one answer here
                // that is allowed below the one row floor `guess` puts in.
                Some(0) => Stat::exact(0, Provenance::ZoneMap),
                Some(ceiling) => capped(guess_from(of(input), kept, from), ceiling),
                None => guess_from(of(input), kept, from),
            }
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
        Node::Join { left, right, kind, conditions, .. } => join(
            of(left),
            of(right),
            kind,
            plan.expr_list(conditions).len(),
            keyspace(plan, conditions, stats),
        ),
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
    guess_from(input, kept, FROM_A_CONSTANT)
}

/// [`guess`] where the fraction did not come from a constant.
///
/// The class is [`GUESSED`] either way. A fraction worked out from a distinct count is still a
/// guess, because it assumes the rows are spread evenly over the values and nothing here has
/// checked that, and a column where they are not is exactly the column somebody wants to find.
/// What changes is the provenance, so that `EXPLAIN` distinguishes a node where a number was read
/// from one where nobody had a number at all, which is the whole reason the field is printed.
fn guess_from(input: Stat<u64>, kept: f64, from: Provenance) -> Stat<u64> {
    match input {
        Stat::Unknown => Stat::Unknown,
        Stat::Known { value, class, .. } => Stat::Known {
            value: scale(value, kept).max(1),
            class: class.combine(GUESSED),
            provenance: from,
        },
    }
}

/// A guess held under a number the answer provably cannot exceed.
///
/// The ceiling comes from the bounds a store keeps per part of itself, so it is a real fact about
/// this filter over this file, and the guess is a constant that knows nothing about either. Where
/// the guess is already under the ceiling it is left alone, because a ceiling says nothing about
/// how far under it the answer sits and overwriting an estimate with a bound would be trading a
/// number for a worse one. Where the ceiling bites, it is the answer and it says so: the value is
/// certain from above and unknown from below, which is what [`CEILING`] means, and it names the
/// zone map rather than the constant it replaced.
///
/// That is the whole reason this is a minimum rather than a new base to take a fraction of. A
/// ceiling is never below the truth, so a minimum of it and the guess is never further from the
/// truth than the guess was. This cannot make an estimate worse, ever, and nothing else that reads
/// these bounds has that property.
fn capped(guessed: Stat<u64>, ceiling: u64) -> Stat<u64> {
    match guessed {
        // Not `Unknown` any more. A filter over a table nobody counted still cannot produce more
        // rows than the parts the bounds leave hold, and that is the same kind of answer a `LIMIT`
        // over an unknown input gives.
        Stat::Unknown => {
            Stat::Known { value: ceiling, class: CEILING, provenance: Provenance::ZoneMap }
        }
        Stat::Known { value, .. } if ceiling < value => {
            Stat::Known { value: ceiling, class: CEILING, provenance: Provenance::ZoneMap }
        }
        known => known,
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

/// What fraction of its input a filter is assumed to keep, and where that fraction came from.
///
/// The product over the conditions. An equality against a constant on a column somebody counted
/// keeps one value out of however many the column holds, which is the uniformity assumption and is
/// the oldest textbook rule there is. Everything else keeps [`KEPT_BY_A_CONDITION`], which is the
/// same constant this had for every condition before.
///
/// One over the count is not always smaller than the constant and is not meant to be. A column of
/// three values gives a third, which is above the fifth the constant guessed, and that is the
/// direction the constant was wrong in for `o_orderstatus`. The rule is to use the number where
/// there is one, not to make the answer smaller.
///
/// The count is the column's own, taken at the scan it comes from, so a filter above a join reads
/// the base table's count and applies it to an input something else has already cut down. That is
/// the standard reading and it is why this stays [`GUESSED`]: it assumes the filter and whatever
/// happened underneath are independent, which is the assumption every estimator makes and the one
/// that fails first.
fn kept(plan: &Plan, predicate: ExprRef, stats: &Statistics) -> (f64, Provenance) {
    let mut fraction = 1.0;
    let (mut counted, mut guessed) = (0_u32, 0_u32);
    for conjunct in conjuncts(plan, predicate) {
        match values(plan, conjunct, stats) {
            Some(values) => {
                fraction /= widened(values);
                counted += 1;
            }
            None => {
                fraction *= KEPT_BY_A_CONDITION;
                guessed += 1;
            }
        }
    }
    // A fraction that is part counted and part guessed came from the arithmetic over the two rather
    // than from either, which is what `Propagation` is for. A reader chasing a bad estimate wants to
    // know which of the three this was without reading the predicate.
    let from = match (counted, guessed) {
        (0, _) => FROM_A_CONSTANT,
        (_, 0) => Provenance::Sketch,
        _ => Provenance::Propagation,
    };
    (fraction, from)
}

/// The conditions a predicate is made of, capped.
///
/// A top level `AND` is the only thing that splits, which is the same split filter pushdown makes
/// and for the same reason: those are the parts that each have to hold. An `OR` is one condition
/// however many branches it has, and something inside a function call is not reached, because
/// `f(a AND b)` is one predicate about whatever `f` does.
///
/// A conjunct with the same value for every row is not here. The selectivity constant is a guess
/// about a predicate over data, and a condition that does not read the data keeps every row or none
/// of them rather than a fifth of them. Most of those are folded away before this ever sees them,
/// and the one that survives is the fold that was abandoned so that the error still comes from
/// running the query.
///
/// Capped at eight so that a query written by a generator does not compound its way to a factor of
/// a million. Past a handful of conditions the product has stopped meaning anything anyway, and the
/// cap is where it stops pretending to.
fn conjuncts(plan: &Plan, predicate: ExprRef) -> Vec<ExprRef> {
    let parts = match *plan.expr(predicate) {
        Expr::Conjunction { op: ConjunctionOp::And, children } => plan.expr_list(children).to_vec(),
        _ => vec![predicate],
    };
    parts.into_iter().filter(|&part| !walk::constant(plan, part)).take(8).collect()
}

/// How many values an equality against a constant picks one of, where anybody counted them.
///
/// Only `=`. The other comparisons are about order rather than about one value, `<>` picks all but
/// one and is the complement of this rather than this, and the two distinctness operators are about
/// nulls, where a distinct count says nothing because the Parquet footer does not count the null as
/// a value.
///
/// `None` rather than a fallback to the row count, which is the fallback [`distinct`] makes for the
/// join arithmetic. One over the rows is one row, so a filter that took it would call every equality
/// on an uncounted column a single row lookup, and `c_mktsegment = 'BUILDING'` would come out at one
/// row instead of thirty thousand. The join can fall back because the row count put through its
/// arithmetic gives the containment assumption back exactly. This has no such identity and has to
/// refuse.
fn values(plan: &Plan, conjunct: ExprRef, stats: &Statistics) -> Option<u64> {
    let Expr::Compare { op: CompareOp::Equal, left, right } = *plan.expr(conjunct) else {
        return None;
    };
    let binding = match (plan.expr(left), plan.expr(right)) {
        (&Expr::Column(binding), _) if walk::constant(plan, right) => binding,
        (_, &Expr::Column(binding)) if walk::constant(plan, left) => binding,
        _ => return None,
    };
    // A column with no values in it is an empty column or a column of nothing but nulls, and
    // neither is something to divide by.
    stated(plan, binding, stats).filter(|&values| values > 0)
}

/// How many rows sit in the parts of `input` that `predicate` cannot rule out.
///
/// `None` unless the filter sits straight on a scan whose store kept bounds and at least one
/// conjunct reads as a test. Straight on, with no projection in between, because a projection
/// renames columns and the name is what the store is asked by, and following one through would be a
/// second place that has to agree with the first about what a column is called.
///
/// A conjunct that is not a test is not a refusal. Dropping it leaves parts in that a full reading
/// would have ruled out, so the answer stays a ceiling, and the guess above it still applies. What
/// is a refusal is a test naming a column the store does not have, which means this plan and this
/// store disagree about what is being read, and a number worked out from that disagreement would
/// rule out parts holding rows the query wants.
///
/// The position is turned into a name and the name is given to the store, rather than the position
/// being handed over directly. Column pruning moves a scan's positions and moves nothing else, so a
/// position is about the plan and the store numbers its columns the way the file does.
fn surviving(plan: &Plan, input: NodeRef, predicate: ExprRef) -> Option<u64> {
    let index = bounds::scanned(plan, input)?;
    let zones = plan.zones(index)?;
    let names = match *plan.node(input) {
        Node::Get { columns, .. } | Node::TableFunction { columns, .. } => plan.field_list(columns),
        _ => return None,
    };
    let read = bounds::of(plan, input, predicate);
    if read.is_empty() {
        return None;
    }
    let mut tests = Vec::with_capacity(read.len());
    for (position, op, value) in read {
        let name = &names.get(position)?.name;
        tests.push(Test { column: zones.column(name)?, op, value });
    }
    zones.surviving(&tests)
}

/// How many distinct values the column a binding names holds, where anybody counted.
///
/// A binding names the operator that produces the column and the position it has there, so this
/// finds the operator and asks what the column at that position is called. Only a scan has an
/// answer, and a projection is followed through to one.
///
/// A column nobody counted falls back to the table's rows. See [`Missing::Rows`] for why, and
/// [`stated`] for the caller that cannot take that answer.
fn distinct(plan: &Plan, binding: ColumnBinding, stats: &Statistics) -> Option<u64> {
    follow(plan, binding, stats, Missing::Rows, 16)
}

/// [`distinct`] restricted to columns somebody actually counted.
fn stated(plan: &Plan, binding: ColumnBinding, stats: &Statistics) -> Option<u64> {
    follow(plan, binding, stats, Missing::Nothing, 16)
}

/// What to answer at a scan for a column nobody counted.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Missing {
    /// The table's rows. A column cannot hold more distinct values than the table has rows, so this
    /// is a bound rather than a guess, and put through the arithmetic in [`join`] it gives back the
    /// containment assumption exactly. That is why the join can fall back rather than refusing: a
    /// refusal on one side would throw away a stated count on the other, and that is most of TPC-H,
    /// where the low cardinality column is stated and the key it joins against is not.
    Rows,
    /// Nothing. For a caller with no such identity, where the row count would come out as a claim
    /// rather than as a fallback.
    Nothing,
}

/// `distinct` with the budget it spends going through projections.
///
/// A projection is followed when the expression at the position is nothing but a reference to a
/// column underneath, because a projection that carries a column through unchanged carries its
/// distinct values through unchanged as well. Every query written against a view goes through one
/// of those, so without this the counts would be read by almost nothing.
///
/// A column that came out of an expression, an aggregate or a set operation stops the search. What
/// a function or a group by did to the number of distinct values under it is not something this
/// knows, and the count of the input is a wrong answer rather than an approximate one. A cast is
/// not followed either: a widening one keeps the distinct values and a narrowing one can merge
/// them, and telling those apart is more than this needs.
///
/// The budget is against a plan that is malformed rather than against one that is deep. A
/// projection over a projection over a projection is ordinary and sixteen of them is not, and the
/// arena invariant means a well formed plan cannot cycle here anyway.
fn follow(
    plan: &Plan,
    binding: ColumnBinding,
    stats: &Statistics,
    missing: Missing,
    depth: u32,
) -> Option<u64> {
    let depth = depth.checked_sub(1)?;
    let position = binding.column as usize;
    let rows = missing == Missing::Rows;
    for at in 0..u32::try_from(plan.node_count()).unwrap_or(u32::MAX) {
        match *plan.node(at) {
            Node::Get { catalog, schema, table, index, columns, .. } if index == binding.table => {
                let name = &plan.field_list(columns).get(position)?.name;
                let catalog = plan.string(catalog);
                let schema = plan.string(schema);
                let table = plan.string(table);
                return stats
                    .distinct_in(catalog, schema, table, name)
                    .or_else(|| rows.then(|| stats.rows_in(catalog, schema, table)).flatten());
            }
            Node::TableFunction { index, columns, .. } if index == binding.table => {
                let name = &plan.field_list(columns).get(position)?.name;
                return plan
                    .distinct_measured(index, name)
                    .or_else(|| rows.then(|| plan.measured(index).decide().copied()).flatten());
            }
            Node::Project { index, exprs, .. } if index == binding.table => {
                let &carried = plan.expr_list(exprs).get(position)?;
                let &Expr::Column(carried) = plan.expr(carried) else {
                    return None;
                };
                return follow(plan, carried, stats, missing, depth);
            }
            _ => {}
        }
    }
    None
}

/// How many pairs of values the conditions of a join can match on, where every one is understood.
///
/// The product over the conditions of the larger of the two sides' distinct counts, which is the
/// standard reading of an equijoin: the two columns draw from a shared set of values, the larger
/// count is how big that set is, and the values are assumed to be spread evenly over it. `None`
/// unless every condition is an equality between two base columns that both have a count, because a
/// condition nobody understood could be the one doing all the work and a divisor that left it out
/// would claim more rows than the join can produce.
fn keyspace(plan: &Plan, conditions: Slice, stats: &Statistics) -> Option<u64> {
    keyspace_of(plan, plan.expr_list(conditions), stats)
}

/// `keyspace` over conditions the caller is holding rather than over a slice of the arena.
///
/// Join ordering wants this. It is deciding which pair to join next and the conditions that would
/// apply at that pair are the ones it has just worked out are testable there, which is a list it
/// built and not a list any node in the arena holds.
#[must_use]
pub fn keyspace_of(plan: &Plan, conditions: &[ExprRef], stats: &Statistics) -> Option<u64> {
    if conditions.is_empty() {
        return None;
    }
    let mut product: u64 = 1;
    for &condition in conditions {
        let Expr::Compare { op: CompareOp::Equal | CompareOp::NotDistinctFrom, left, right } =
            *plan.expr(condition)
        else {
            return None;
        };
        let (&Expr::Column(left), &Expr::Column(right)) = (plan.expr(left), plan.expr(right))
        else {
            return None;
        };
        let pair = distinct(plan, left, stats)?.max(distinct(plan, right, stats)?);
        product = product.checked_mul(pair)?;
    }
    // A column with no distinct values at all is an empty column or a column of nothing but nulls,
    // and neither is something to divide by.
    (product > 0).then_some(product)
}

/// How many rows an equijoin of two sides of these sizes over that many key values produces.
///
/// The containment assumption: every row of the smaller side finds a match, so an equijoin produces
/// about as many rows as its larger side. It is the standard guess and it is right whenever one side
/// of the condition is a key, which is most joins anybody writes and none of the joins that hurt.
///
/// Where the key has a distinct count, the join can also be counted directly. Each side spreads its
/// rows over the same set of key values, so a value gets `left / keys` rows from one side and
/// `right / keys` from the other, and the pairs come to `left * right / keys`. On a key that is a
/// key the two agree: a thousand rows joined to a million on a column with a million values is a
/// million rows either way. On a column with twenty five values in it they do not agree at all, and
/// the second one is right. TPC-H q5 joins a hundred and fifty thousand customers to ten thousand
/// suppliers on a nation, and the containment assumption calls that a hundred and fifty thousand
/// rows when it is sixty million.
///
/// The larger of the two is taken rather than the second one outright, so this can only ever raise
/// an estimate above what shape alone said. A distinct count larger than the rows on the smaller
/// side is the case where it would lower one, and that happens when the count came from a table
/// that a filter underneath has already cut down, which makes the count stale rather than the join
/// small.
///
/// Public because the join ordering pass scores an order with it. The two have to be one function
/// rather than two that agree today, since an order chosen by one arithmetic and kept by another is
/// an order nobody can reason about.
#[must_use]
pub fn matched(left: u64, right: u64, keys: Option<u64>) -> u64 {
    let counted = keys.map_or(0, |keys| left.saturating_mul(right) / keys);
    left.max(right).max(counted)
}

/// The join kinds, each of which is a different question.
fn join(
    left: Stat<u64>,
    right: Stat<u64>,
    kind: JoinKind,
    conditions: usize,
    keys: Option<u64>,
) -> Stat<u64> {
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
            let matched = matched(left, right, keys);
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

/// A count as a divisor, at least one so that nothing divides by zero or grows.
#[expect(
    clippy::cast_precision_loss,
    reason = "a count past two to the fifty third is not a count anybody measured"
)]
fn widened(count: u64) -> f64 {
    (count as f64).max(1.0)
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
    use std::sync::{Arc, Mutex};

    use rudb_common::bounds::{Op, Test, Zones};
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

    /// The estimate against table sizes and distinct counts, the counts named table then column.
    fn counted(text: &str, tables: &[(&str, u64)], columns: &[(&str, &str, u64)]) -> Option<u64> {
        let mut stats = statistics(tables);
        for (table, column, distinct) in columns {
            stats.record_distinct("memory", "main", table, column, *distinct);
        }
        let plan =
            Plan::parse(text).unwrap_or_else(|error| panic!("{text} did not parse: {error}"));
        rows(&plan, plan.root(), &stats)
    }

    /// The same estimate with the class and the provenance still attached.
    fn counted_stat(
        text: &str,
        tables: &[(&str, u64)],
        columns: &[(&str, &str, u64)],
    ) -> Stat<u64> {
        let mut stats = statistics(tables);
        for (table, column, distinct) in columns {
            stats.record_distinct("memory", "main", table, column, *distinct);
        }
        let plan =
            Plan::parse(text).unwrap_or_else(|error| panic!("{text} did not parse: {error}"));
        rows_stat(&plan, plan.root(), &stats)
    }

    /// A filter of the given predicate over a two column scan of `t`.
    fn filtered(predicate: &str) -> String {
        format!(
            "Filter {predicate}\n  Get memory.main.t AS t #0 [a::INTEGER, b::INTEGER]\n"
        )
    }

    /// A join of two one column scans on their one column, which is what the counted tests sit on.
    fn joined(left: &str, right: &str) -> String {
        format!(
            "Join INNER on=[(#0.0::INTEGER = #1.0::INTEGER)::BOOLEAN]\n  {}  {}",
            scan(left, 0),
            scan(right, 1)
        )
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

    #[test]
    fn a_join_on_a_column_with_few_values_in_it_produces_more_rows_than_its_larger_side() {
        // TPC-H q5 written small: customers joined to suppliers on a nation key with twenty five
        // values in it. The containment assumption calls this a hundred and fifty thousand rows
        // and the real answer is sixty million, which is the estimate that made the join ordering
        // pass pick the worst order it could find.
        let text = joined("customer", "supplier");
        let tables = &[("customer", 150_000), ("supplier", 10_000)];
        assert_eq!(counted(&text, tables, &[]), Some(150_000));
        let counts = &[("customer", "a", 25), ("supplier", "a", 25)];
        assert_eq!(counted(&text, tables, counts), Some(60_000_000));
    }

    #[test]
    fn a_join_on_a_key_is_the_containment_assumption_and_a_count_does_not_change_it() {
        // Every order belongs to one customer, so the join is as tall as the orders however the
        // number is arrived at. The two readings agree here, which is why the containment
        // assumption survived as long as it did.
        let text = joined("customer", "orders");
        let tables = &[("customer", 150_000), ("orders", 1_500_000)];
        assert_eq!(counted(&text, tables, &[]), Some(1_500_000));
        let counts = &[("customer", "a", 150_000), ("orders", "a", 150_000)];
        assert_eq!(counted(&text, tables, counts), Some(1_500_000));
    }

    #[test]
    fn a_count_larger_than_the_rows_on_the_smaller_side_does_not_shrink_the_estimate() {
        // A count that big means the table it was taken on has been cut down by something
        // underneath since anybody counted, which makes the count stale rather than the join
        // small. The larger of the two readings is taken so a stale count cannot lower anything.
        let text = joined("small", "big");
        let tables = &[("small", 10), ("big", 1_000)];
        let counts = &[("small", "a", 1_000_000), ("big", "a", 1_000_000)];
        assert_eq!(counted(&text, tables, counts), Some(1_000));
    }

    #[test]
    fn two_conditions_match_on_the_pairs_of_values_and_not_on_either_column() {
        // Ten values on one column and ten on the other is a hundred pairs, and the rows spread
        // over the pairs rather than over either column alone.
        let text = concat!(
            "Join INNER on=[(#0.0::INTEGER = #1.0::INTEGER)::BOOLEAN, ",
            "(#0.1::INTEGER = #1.1::INTEGER)::BOOLEAN]\n",
            "  Get memory.main.l AS l #0 [a::INTEGER, b::INTEGER]\n",
            "  Get memory.main.r AS r #1 [a::INTEGER, b::INTEGER]\n"
        );
        let tables = &[("l", 1_000_000), ("r", 1_000_000)];
        let counts = &[("l", "a", 10), ("l", "b", 10), ("r", "a", 10), ("r", "b", 10)];
        assert_eq!(counted(text, tables, counts), Some(10_000_000_000));
    }

    #[test]
    fn a_condition_that_is_not_an_equality_between_two_columns_leaves_the_counts_unread() {
        // A range condition could be the one doing all the work, and a divisor that left it out
        // would claim more rows than the join can produce. Nothing is divided by at all here.
        let text = concat!(
            "Join INNER on=[(#0.0::INTEGER < #1.0::INTEGER)::BOOLEAN]\n",
            "  Get memory.main.l AS l #0 [a::INTEGER]\n",
            "  Get memory.main.r AS r #1 [a::INTEGER]\n"
        );
        let tables = &[("l", 150_000), ("r", 10_000)];
        let counts = &[("l", "a", 25), ("r", "a", 25)];
        assert_eq!(counted(text, tables, counts), Some(150_000));
    }

    #[test]
    fn a_projection_that_carries_a_column_through_carries_its_count_through_as_well() {
        // Every query written against a view goes through one of these, so without this the
        // counts would be read by almost nothing.
        let text = concat!(
            "Join INNER on=[(#1.0::INTEGER = #3.0::INTEGER)::BOOLEAN]\n",
            "  Project #1 [#0.0::INTEGER AS a]\n",
            "    Get memory.main.customer AS customer #0 [a::INTEGER]\n",
            "  Project #3 [#2.0::INTEGER AS a]\n",
            "    Get memory.main.supplier AS supplier #2 [a::INTEGER]\n"
        );
        let tables = &[("customer", 150_000), ("supplier", 10_000)];
        let counts = &[("customer", "a", 25), ("supplier", "a", 25)];
        assert_eq!(counted(text, tables, counts), Some(60_000_000));
    }

    #[test]
    fn a_projection_that_computes_something_is_where_the_count_stops() {
        // What the addition did to the number of distinct values under it is not something this
        // knows, and the count of the input is a wrong answer rather than an approximate one.
        let text = concat!(
            "Join INNER on=[(#1.0::INTEGER = #3.0::INTEGER)::BOOLEAN]\n",
            "  Project #1 [\"+\"(#0.0::INTEGER, 1::INTEGER)::INTEGER AS a]\n",
            "    Get memory.main.customer AS customer #0 [a::INTEGER]\n",
            "  Project #3 [#2.0::INTEGER AS a]\n",
            "    Get memory.main.supplier AS supplier #2 [a::INTEGER]\n"
        );
        let tables = &[("customer", 150_000), ("supplier", 10_000)];
        let counts = &[("customer", "a", 25), ("supplier", "a", 25)];
        assert_eq!(counted(text, tables, counts), Some(150_000));
    }

    #[test]
    fn a_column_with_no_distinct_values_at_all_is_not_divided_by() {
        // An empty column or a column of nothing but nulls. Neither is something to divide by, and
        // the estimate falls back to the shape it used before there were any counts.
        let text = joined("l", "r");
        let tables = &[("l", 1_000), ("r", 1_000)];
        let counts = &[("l", "a", 0), ("r", "a", 0)];
        assert_eq!(counted(&text, tables, counts), Some(1_000));
    }

    #[test]
    fn an_equality_against_a_constant_keeps_one_value_out_of_the_count() {
        // TPC-H o_clerk written small. A fifth of three hundred thousand orders is sixty thousand
        // and the answer is fifteen hundred, which is the error this rule exists to remove.
        let text = filtered("(#0.0::INTEGER = 3::INTEGER)::BOOLEAN");
        let tables = &[("t", 300_000)];
        assert_eq!(counted(&text, tables, &[]), Some(60_000));
        assert_eq!(counted(&text, tables, &[("t", "a", 200)]), Some(1_500));
    }

    #[test]
    fn a_count_that_says_more_rows_than_the_constant_did_is_still_the_count() {
        // Three values is a third, which is above the fifth the constant guessed. The rule is to
        // use the number where there is one rather than to make the answer smaller, and this is
        // the direction the constant was wrong in for o_orderstatus.
        let text = filtered("(#0.0::INTEGER = 3::INTEGER)::BOOLEAN");
        assert_eq!(counted(&text, &[("t", 900_000)], &[("t", "a", 3)]), Some(300_000));
    }

    #[test]
    fn two_counted_equalities_divide_by_both_counts() {
        // The independence assumption, which is the same one the constant made when it multiplied
        // two fifths together, with the counts standing in for the fifths.
        let text = filtered(
            "((#0.0::INTEGER = 3::INTEGER)::BOOLEAN AND (#0.1::INTEGER = 4::INTEGER)::BOOLEAN)::BOOLEAN",
        );
        let tables = &[("t", 200_000)];
        let counts = &[("t", "a", 25), ("t", "b", 40)];
        assert_eq!(counted(&text, tables, counts), Some(200));
    }

    #[test]
    fn a_column_nobody_counted_keeps_the_constant_rather_than_becoming_one_row() {
        // One over the rows is one row, so a fallback to the row count here would call every
        // equality on an uncounted column a single row lookup. The count on the other column is
        // still read, so one uncounted condition does not throw away the one next to it.
        let text = filtered(
            "((#0.0::INTEGER = 3::INTEGER)::BOOLEAN AND (#0.1::INTEGER = 4::INTEGER)::BOOLEAN)::BOOLEAN",
        );
        let tables = &[("t", 1_000_000)];
        assert_eq!(counted(&text, tables, &[]), Some(40_000));
        assert_eq!(counted(&text, tables, &[("t", "a", 50)]), Some(4_000));
    }

    #[test]
    fn only_an_equality_against_a_constant_reads_the_count() {
        // A range is about order rather than about one value, and two columns comparing to each
        // other pick a value neither count describes. Both keep the constant with the count sitting
        // right there unread.
        let tables = &[("t", 1_000_000)];
        let counts = &[("t", "a", 50), ("t", "b", 50)];
        let above = filtered("(#0.0::INTEGER > 3::INTEGER)::BOOLEAN");
        assert_eq!(counted(&above, tables, counts), Some(200_000));
        let other = filtered("(#0.0::INTEGER <> 3::INTEGER)::BOOLEAN");
        assert_eq!(counted(&other, tables, counts), Some(200_000));
        let columns = filtered("(#0.0::INTEGER = #0.1::INTEGER)::BOOLEAN");
        assert_eq!(counted(&columns, tables, counts), Some(200_000));
    }

    #[test]
    fn an_equality_reads_the_count_whichever_side_the_constant_is_on() {
        let text = filtered("(3::INTEGER = #0.0::INTEGER)::BOOLEAN");
        assert_eq!(counted(&text, &[("t", 100_000)], &[("t", "a", 50)]), Some(2_000));
    }

    #[test]
    fn a_projection_carries_a_count_up_to_a_filter_as_well() {
        // The same walk the join arithmetic makes, which matters here because every query written
        // against a view puts a projection between the filter and the scan that counted.
        let text = concat!(
            "Filter (#1.0::INTEGER = 3::INTEGER)::BOOLEAN\n",
            "  Project #1 [#0.0::INTEGER AS a]\n",
            "    Get memory.main.t AS t #0 [a::INTEGER]\n"
        );
        assert_eq!(counted(text, &[("t", 100_000)], &[("t", "a", 50)]), Some(2_000));
    }

    #[test]
    fn a_count_bigger_than_the_table_still_leaves_a_row() {
        // The floor the whole module has always had. A plan that believes a subtree produces no
        // rows is a plan that stops reading it, and a stale count is not a proof of anything.
        let text = filtered("(#0.0::INTEGER = 3::INTEGER)::BOOLEAN");
        assert_eq!(counted(&text, &[("t", 10)], &[("t", "a", 1_000_000)]), Some(1));
    }

    #[test]
    fn where_a_filter_got_its_fraction_from_is_printed() {
        // Three answers, so that somebody reading EXPLAIN and chasing a bad estimate can tell
        // which of the three this was without reading the predicate back.
        let tables = &[("t", 1_000_000)];
        let one = filtered("(#0.0::INTEGER = 3::INTEGER)::BOOLEAN");
        assert_eq!(
            counted_stat(&one, tables, &[("t", "a", 50)]).provenance(),
            Some(Provenance::Sketch)
        );
        assert_eq!(counted_stat(&one, tables, &[]).provenance(), Some(Provenance::Default));
        let both = filtered(
            "((#0.0::INTEGER = 3::INTEGER)::BOOLEAN AND (#0.1::INTEGER > 4::INTEGER)::BOOLEAN)::BOOLEAN",
        );
        assert_eq!(
            counted_stat(&both, tables, &[("t", "a", 50)]).provenance(),
            Some(Provenance::Propagation)
        );
    }

    #[test]
    fn a_counted_filter_is_still_a_guess() {
        // The count is a fact about the column and the fraction taken from it is not a fact about
        // the query. It assumes the values are spread evenly and that the filter is independent of
        // whatever happened underneath, which is the assumption that fails first.
        let text = filtered("(#0.0::INTEGER = 3::INTEGER)::BOOLEAN");
        let stat = counted_stat(&text, &[("t", 1_000_000)], &[("t", "a", 50)]);
        assert_eq!(stat.decide(), Some(&20_000));
        assert_eq!(stat.enable(), None);
        assert_eq!(stat.answer(), None);
    }

    /// A store of two columns, `a` then `b`, answering with a count fixed when it is built.
    ///
    /// The columns are deliberately in the other order from the scan above, because that is the
    /// case the name mapping exists for. A plan numbers a scan's columns by where they sit in what
    /// the scan produces, column pruning moves that, and the file's own order never moves. Handing
    /// the plan's position straight to the store would test the wrong column.
    #[derive(Debug)]
    struct Stub {
        /// What [`Zones::surviving`] answers, whatever it is asked.
        surviving: Option<u64>,
        /// Every test it was asked, so a test can check which column the estimator named.
        asked: Mutex<Vec<Test>>,
    }

    impl Stub {
        fn new(surviving: Option<u64>) -> Arc<Self> {
            Arc::new(Self { surviving, asked: Mutex::new(Vec::new()) })
        }
    }

    impl Zones for Stub {
        fn column(&self, name: &str) -> Option<usize> {
            match name {
                "b" => Some(0),
                "a" => Some(1),
                _ => None,
            }
        }

        fn surviving(&self, tests: &[Test]) -> Option<u64> {
            self.asked.lock().expect("no test panics while holding this").extend_from_slice(tests);
            self.surviving
        }
    }

    /// A two column scan, whose columns the stub above numbers the other way round.
    fn bounded_scan() -> String {
        "Get memory.main.t AS t #0 [a::INTEGER, b::INTEGER]\n".to_string()
    }

    /// The estimate for a plan whose table zero is the given store.
    fn zoned(text: &str, rows: u64, zones: &Arc<Stub>) -> Stat<u64> {
        let stats = statistics(&[("t", rows)]);
        let mut plan =
            Plan::parse(text).unwrap_or_else(|error| panic!("{text} did not parse: {error}"));
        plan.set_zones(0, Arc::clone(zones) as Arc<dyn Zones>);
        rows_stat(&plan, plan.root(), &stats)
    }

    #[test]
    fn a_filter_the_bounds_rule_out_entirely_is_exactly_no_rows() {
        // The one answer bounds give that is a fact rather than a guess. No row group holds a
        // value the constant falls inside, so there is no row to produce, and saying so exactly
        // lets everything above it plan against zero instead of against a fifth of the file.
        let text = format!("Filter (#0.0::INTEGER > 9::INTEGER)::BOOLEAN\n  {}", bounded_scan());
        let zones = Stub::new(Some(0));
        assert_eq!(zoned(&text, 10_000_000, &zones), Stat::exact(0, Provenance::ZoneMap));
    }

    #[test]
    fn a_ceiling_below_the_guess_replaces_it_and_says_it_is_a_ceiling() {
        // On the ten million row smoke file `id < 1000` guesses two million and the bounds leave
        // 122,880, against a truth of a thousand. The number the planner gets is the smaller one
        // and it is marked as a bound rather than as an estimate, because that is what it is: the
        // answer is somewhere between no rows and this, and no guess is involved in the ceiling.
        let text = format!("Filter (#0.0::INTEGER < 9::INTEGER)::BOOLEAN\n  {}", bounded_scan());
        let zones = Stub::new(Some(100_000));
        let capped = zoned(&text, 10_000_000, &zones);
        assert_eq!(capped.value(), Some(&100_000));
        assert_eq!(
            capped.class(),
            Some(Class::Certified { bound: 1.0, direction: Direction::AtMost })
        );
        assert_eq!(capped.provenance(), Some(Provenance::ZoneMap));
        // A ceiling of one row is allowed, unlike the guess, which floors at one for a different
        // reason: the guess floors because a relation estimated away is a subtree nobody reads,
        // and a ceiling of one is a fact that happens to be one.
        let tiny = Stub::new(Some(1));
        assert_eq!(zoned(&text, 10_000_000, &tiny).value(), Some(&1));
    }

    #[test]
    fn a_ceiling_above_the_guess_leaves_the_guess_alone() {
        // This is what makes the whole thing safe. A bound says nothing about how far under it the
        // answer sits, so replacing a guess that is already below it would be trading a number for
        // a worse one. `k = 42` on the smoke file is the case: every row group holds a 42 in its
        // range, the bounds rule nothing out, and the estimate stays the two million it was.
        let text = format!("Filter (#0.0::INTEGER < 9::INTEGER)::BOOLEAN\n  {}", bounded_scan());
        let wide = Stub::new(Some(10_000_000));
        assert_eq!(
            zoned(&text, 10_000_000, &wide),
            Stat::estimated(2_000_000, Provenance::Default)
        );
    }

    #[test]
    fn a_store_that_cannot_answer_leaves_the_estimate_exactly_as_it_was() {
        let text = format!("Filter (#0.0::INTEGER < 9::INTEGER)::BOOLEAN\n  {}", bounded_scan());
        let quiet = Stub::new(None);
        assert_eq!(zoned(&text, 1_000_000, &quiet), stat(&text, &[("t", 1_000_000)]));
        assert_eq!(zoned(&text, 1_000_000, &quiet), Stat::estimated(200_000, Provenance::Default));
    }

    #[test]
    fn a_filter_that_reads_as_no_test_at_all_does_not_ask_the_store() {
        // Asking with no tests would come back with the whole file, which is the right number and
        // the wrong provenance: nothing was ruled out by any bound, so nothing should claim to
        // have been. A comparison of two columns is the case, since bounds on one say nothing
        // about the other's value in the same row.
        let text = format!("Filter (#0.0::INTEGER = #0.1::INTEGER)::BOOLEAN\n  {}", bounded_scan());
        let zones = Stub::new(Some(7));
        assert_eq!(zoned(&text, 1_000_000, &zones), Stat::estimated(200_000, Provenance::Default));
        assert!(zones.asked.lock().expect("not poisoned").is_empty(), "it was never asked");
    }

    #[test]
    fn the_column_the_store_is_asked_about_is_the_one_the_plan_named_and_not_the_position() {
        // The bug this mapping exists to stop. The plan's column zero is `a` and the store's
        // column zero is `b`, so a test handed straight through would rule out row groups on the
        // wrong column's bounds. That drops rows the query wanted, which is a wrong answer and not
        // a slow one.
        let text = format!("Filter (#0.0::INTEGER < 9::INTEGER)::BOOLEAN\n  {}", bounded_scan());
        let zones = Stub::new(Some(100));
        zoned(&text, 1_000_000, &zones);
        let asked = zones.asked.lock().expect("not poisoned");
        assert_eq!(asked.len(), 1);
        assert_eq!(asked[0].column, 1, "`a` is the store's column one");
        assert_eq!(asked[0].op, Op::Less);
    }

    #[test]
    fn a_table_with_no_store_recorded_is_estimated_the_way_it_always_was() {
        // Which is every table today except a `read_parquet` of one file, so this is the path
        // almost every query still takes and it has to be untouched.
        let text = format!("Filter (#0.0::INTEGER < 9::INTEGER)::BOOLEAN\n  {}", bounded_scan());
        assert_eq!(stat(&text, &[("t", 1_000_000)]), Stat::estimated(200_000, Provenance::Default));
    }
}
