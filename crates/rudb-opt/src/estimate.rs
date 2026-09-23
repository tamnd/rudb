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
//! part `spec/09-optimizer.md` section 9.3 actually specifies and it needs the facts that
//! M3's storage layer collects, so it waits for them.

use std::collections::BTreeMap;
use std::sync::Arc;

use rudb_common::bounds::{Bound, Frequencies, Spread, Test, Zones};
use rudb_common::stat::{Class, Direction, Provenance, Stat, Use};
use rudb_common::{Field, Value};
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

/// What one column of one table is known by, which is the whole key space of [`Facts`].
///
/// One key type and one `get` rather than a reader per kind of number, because the rule about what
/// a missing answer means has to be in one place. Two accessors are two chances to write `0` where
/// `Unknown` belongs.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub enum Key<'a> {
    /// How many rows that table holds.
    Rows {
        /// The database it is attached in.
        catalog: &'a str,
        /// The schema it is in.
        schema: &'a str,
        /// The table.
        table: &'a str,
    },
    /// How many distinct values that column of that table holds.
    Distinct {
        /// The database it is attached in.
        catalog: &'a str,
        /// The schema it is in.
        schema: &'a str,
        /// The table.
        table: &'a str,
        /// The column, by name, because the position a column has in a scan is whatever column
        /// pruning left and the name is not moved by anything.
        column: &'a str,
    },
}

/// Everything counted that the optimizer was handed, by table and by column.
///
/// Called facts rather than statistics because that is what these are. Every number in here was
/// counted rather than sampled or guessed, the estimates are what this module makes out of them,
/// and the two want different names or a reader has to work out which one a variable holds.
///
/// A side table rather than a field on [`Node::Get`], and a plain count rather than a handle on the
/// catalog. Both of those are so that a plan stays a value: the optimizer's own tests build plans
/// out of text with no database anywhere near them, `Plan::parse` of a printed plan gives back the
/// plan it was printed from, and neither of those survives a node that carries a number only a
/// live catalog could have filled in.
///
/// # Reading it never waits
///
/// [`Facts::get`] is a lookup in a map this value owns, so there is no lock to take, nothing to
/// fault in and nothing to wait for. That is the guarantee `spec/stats/04-in-memory.md` asks for and
/// it is a property of the shape rather than of the code: a set of facts is built once, is never
/// written to again, and is handed to a statement behind an [`std::sync::Arc`]. A key nobody filled
/// in answers [`Stat::Unknown`], which every caller already has to handle, rather than going and
/// finding out and making the planner wait while it does.
///
/// # One generation per plan
///
/// [`Facts::generation`] says which version of the catalog these were read from, and a plan is
/// planned from exactly one of them. Two statements that see the same number are looking at the
/// same catalog, so the second reuses what the first built instead of walking every table and every
/// column again. That walk is what this used to cost per statement whether the query touched those
/// tables or not.
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
pub struct Facts {
    tables: BTreeMap<(String, String, String), u64>,
    columns: BTreeMap<(String, String, String, String), (u64, Provenance)>,
    generation: u64,
}

impl Facts {
    /// Nothing known about anything, and read from no catalog.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Nothing known about anything yet, to be filled in from that version of the catalog.
    #[must_use]
    pub fn at(generation: u64) -> Self {
        Self { generation, ..Self::default() }
    }

    /// Which version of the catalog these were read from, or zero for a set built by hand.
    ///
    /// Zero never matches a real catalog, whose own count starts at one, so a set built in a test
    /// can never be mistaken for one that is still current.
    #[must_use]
    pub const fn generation(&self) -> u64 {
        self.generation
    }

    /// What is known about one key, without waiting for anything.
    ///
    /// A number nobody recorded comes back [`Stat::Unknown`] rather than as a zero or as a guess,
    /// which is the distinction `spec/stats/02-the-catalogue.md` section 2.1.1 exists to keep. A
    /// scan of a table nobody measured and a scan of an empty table are not the same thing, and an
    /// optimizer that confuses them will build a hash table from the side it thinks has no rows.
    ///
    /// What comes back is [`Class::Exact`], because these are counted rather than estimated, and
    /// the provenance says which count it is so that a reader of `EXPLAIN` can tell the two apart.
    /// A distinct count carries the provenance whoever recorded it gave, which is a dictionary for a
    /// file and a sketch for a table in memory, and both are exact or they would not be here.
    #[must_use]
    pub fn get(&self, key: &Key<'_>) -> Stat<u64> {
        let (found, provenance) = match *key {
            Key::Rows { catalog, schema, table } => {
                (self.rows_in(catalog, schema, table), Provenance::RowCount)
            }
            Key::Distinct { catalog, schema, table, column } => {
                match self.distinct_in(catalog, schema, table, column) {
                    Some((value, provenance)) => (Some(value), provenance),
                    None => (None, Provenance::Dictionary),
                }
            }
        };
        found.map_or(Stat::Unknown, |value| Stat::exact(value, provenance))
    }

    /// Record what one table held.
    pub fn record(&mut self, catalog: &str, schema: &str, table: &str, rows: u64) {
        self.tables.insert((catalog.to_owned(), schema.to_owned(), table.to_owned()), rows);
    }

    /// What that table held, where anybody said.
    ///
    /// Private, because [`Facts::get`] is the one reader and the rule about what a missing answer
    /// means belongs in one place. This is the map lookup under it.
    fn rows_in(&self, catalog: &str, schema: &str, table: &str) -> Option<u64> {
        self.tables.get(&(catalog.to_owned(), schema.to_owned(), table.to_owned())).copied()
    }

    /// Record how many distinct values one column of one table holds.
    ///
    /// By name and not by position, because the position a column has in a scan is whatever is left
    /// after column pruning moved it and the name is not moved by anything.
    ///
    /// The provenance rides along because only the caller knows it. Two kinds of table answer this
    /// now and they count in different ways, and the point of printing a provenance in `EXPLAIN` is
    /// to say which. Only an exact count belongs here whichever it is: [`Facts::get`] hands back
    /// what it holds as [`Class::Exact`] and has no way to say anything else.
    pub fn record_distinct(
        &mut self,
        catalog: &str,
        schema: &str,
        table: &str,
        column: &str,
        distinct: u64,
        provenance: Provenance,
    ) {
        let key = (catalog.to_owned(), schema.to_owned(), table.to_owned(), column.to_owned());
        self.columns.insert(key, (distinct, provenance));
    }

    /// How many distinct values that column holds, where anybody counted.
    ///
    /// Private for the same reason [`Facts::rows_in`] is.
    fn distinct_in(
        &self,
        catalog: &str,
        schema: &str,
        table: &str,
        column: &str,
    ) -> Option<(u64, Provenance)> {
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
pub(crate) const FROM_A_CONSTANT: Provenance = Provenance::Default;

/// The class of a number that is a proven ceiling with nothing under it.
///
/// A `LIMIT 10` over an unknown input produces somewhere between no rows and ten, so the value is
/// certain from above and the relative error can be the whole of it, which is a bound of one. That
/// is the weakest certificate there is and it is still worth telling apart from a guess: a guess
/// can be exceeded and this cannot.
const CEILING: Class = Class::Certified { bound: 1.0, direction: Direction::AtMost };

/// What a row count in a plan is read for, in the vocabulary of `spec/stats/05-every-query.md`
/// section 5.1.1.
///
/// One constant rather than a `.decide()` written out at each call site, because that section asks
/// `EXPLAIN` to print which use happened, and a word printed in one file about a call made in
/// another is a word that goes stale the first time somebody changes the call.
///
/// It is [`Use::Decide`] for every cardinality the optimizer reads today and that is not a gap.
/// The three passes that read one, join order in `crate::order`, build side in `crate::sides` and
/// semi lowering in `crate::semi`, each choose between plans that produce the same rows, which is
/// exactly what Decide means. The worst a wrong number does there is a slow query. The day a pass
/// reads a cardinality to license a rewrite instead, that read declares [`Use::Enable`], the class
/// rule refuses everything but an exact count, and the statistics section says an enable happened.
pub const CARDINALITY: Use = Use::Decide;

/// What a distinct count in a plan is read for, in the same vocabulary.
///
/// [`Use::Decide`] as well, and for a stronger reason than the cardinality has: the two callers are
/// an equality's selectivity and a join's cardinality, and both of them are working out how many
/// rows an operator produces so that something can choose a plan. Neither changes what the query
/// answers. So a certified lower bound is allowed through, which is most of what a Parquet footer
/// can supply, and the number being a bound rather than a count costs a plan choice at worst.
///
/// `COUNT(DISTINCT c)` is the read that would declare [`Use::Answer`] here, and it is not written
/// yet. When it is, the class rule refuses everything but an exact count, which is a file of one row
/// group or a native table's dictionary, per `spec/stats/05-every-query.md` section 5.1.
pub const DISTINCT: Use = Use::Decide;

/// How a frequency count is read: to pick between plans, like every other number here.
///
/// The same [`Use::Decide`] as [`DISTINCT`] and for the same reason, even though the count itself is
/// exact where it exists. What it is used for is a filter's estimate, and an estimate that turns out
/// wrong picks a worse plan rather than printing a wrong answer. An exact count read to answer a
/// query would declare [`Use::Answer`] and go through the class rule, and nothing does that yet.
const COMMON: Use = Use::Decide;

/// How a null count is read, which is the same [`Use::Decide`] and for the same reason as [`COMMON`].
///
/// Exact where it exists, used to pick between plans, so a wrong one costs a worse plan and never a
/// wrong answer. `COUNT(column)` is the read that would want [`Use::Answer`] and it does not ask
/// here yet.
const NULLS: Use = Use::Decide;

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
pub fn rows(plan: &Plan, node: NodeRef, stats: &Facts) -> Option<u64> {
    rows_stat(plan, node, stats).read(CARDINALITY).copied()
}

/// How many rows this node would produce if no filter underneath it threw any away.
///
/// The containment assumption a join is estimated by is a statement about two whole tables: every
/// value on the smaller side turns up on the larger one, so the join is as tall as the larger side.
/// A filter under one of the sides breaks it, and breaks it in the direction that matters, because
/// the rows the filter removed are rows the other side no longer matches. So the join is estimated
/// from what the two sides would be without their filters, which is what this answers, and the
/// fractions the filters kept are applied to the result. TPC-H q9 is the case: joining six million
/// lineitem rows to the twenty thousand parts whose name contains a word produces about a fifth of
/// what joining them to all two hundred thousand parts would, and containment on its own says the
/// two are the same size and puts the join that removes nothing first.
///
/// Only the operators a filter can hide under are walked, which is a filter itself, the ones that
/// change the width and not the height, the inner joins and the two join kinds that are a filter
/// written as a join. Everything else is as tall as it is, so it answers with its ordinary estimate
/// and the walk stops there.
///
/// The walk has to agree with itself across the passes that move an operator, because the join
/// ordering pass reads it and the sequence of passes is required to settle after one run. A semi
/// join is where that bites. It starts life as a mark join and ends up underneath whatever inner
/// join it can be pushed below, so the same side is a filtered scan before the semi passes have run
/// and a semi join over a filtered scan afterwards, and the two have to answer the same or the
/// ordering pass decides one thing on the first run and another on the second.
#[must_use]
pub fn unfiltered(plan: &Plan, node: NodeRef, stats: &Facts) -> Stat<u64> {
    match *plan.node(node) {
        // The whole point: the rows it was given rather than the rows it kept.
        Node::Filter { input, .. } => unfiltered(plan, input, stats),
        Node::Project { input, .. }
        | Node::Window { input, .. }
        | Node::Sort { input, .. }
        | Node::Fetch { input, .. }
        | Node::TableFetch { input, .. } => unfiltered(plan, input, stats),
        // A semi join and an anti join keep some of the rows their left side gave them and add
        // nothing, which is what a filter does, so the unfiltered side is the one underneath. They
        // are walked through rather than estimated for the same reason a filter is, and without
        // this a plan where a semi join has been pushed under one estimates the side at what the
        // semi join kept and a plan where it has not estimates the same side at the whole table.
        Node::Join { left, kind: JoinKind::Semi | JoinKind::Anti, .. } => {
            unfiltered(plan, left, stats)
        }
        // A join of two unfiltered sides, which is the containment reading with nothing scaled.
        Node::Join { left, right, kind: kind @ JoinKind::Inner, conditions, .. } => join(
            both(unfiltered(plan, left, stats)),
            both(unfiltered(plan, right, stats)),
            kind,
            plan.expr_list(conditions).len(),
            keyspace(plan, conditions, stats, &mut Vec::new()),
        ),
        Node::CrossProduct { left, right } => {
            unfiltered(plan, left, stats).zip(unfiltered(plan, right, stats), u64::saturating_mul)
        }
        _ => rows_stat(plan, node, stats),
    }
}

/// A side of a join, as the rows it produces and the rows it would produce unfiltered.
///
/// `None` where either is unknown, since a caller that cannot have the first has nothing to score
/// and a caller that cannot have the second has no fraction to apply.
#[must_use]
pub fn side(plan: &Plan, node: NodeRef, stats: &Facts) -> Option<Side> {
    let rows = *rows_stat(plan, node, stats).read(CARDINALITY)?;
    let base = *unfiltered(plan, node, stats).read(CARDINALITY)?;
    Some(Side { rows, base: base.max(rows) })
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
pub fn rows_stat(plan: &Plan, node: NodeRef, stats: &Facts) -> Stat<u64> {
    rows_stat_into(plan, node, stats, &mut Vec::new())
}

/// [`rows_stat`] with the distinct counts read at this node collected into `reads`.
///
/// At this node and not under it. The recursion into the children goes through [`rows_stat`], which
/// throws its own collection away, so a caller that walks every operator of a plan and asks about
/// each of them ends up with each read counted once rather than once per ancestor. That is the same
/// rule `EXPLAIN (STATISTICS)` already counts cardinalities by, and it is the only rule under which
/// the two lines of that section can be read against each other.
///
/// The cardinality this returns is not pushed. A cardinality is the caller's own answer, so the
/// caller records it under [`CARDINALITY`] itself, and only the distinct counts, which are read here
/// and never surface, need carrying out.
#[must_use]
pub fn rows_stat_into(
    plan: &Plan,
    node: NodeRef,
    stats: &Facts,
    reads: &mut Vec<Stat<u64>>,
) -> Stat<u64> {
    let of = |child: NodeRef| rows_stat(plan, child, stats);
    match *plan.node(node) {
        // One row with no columns, which is what a `SELECT` with no `FROM` is bound against.
        Node::Dummy => Stat::exact(1, Provenance::RowCount),
        // The catalog counted these rather than estimating them, so the count is the count. That
        // is the one exact number a plan starts from today and it is why the histogram does not
        // read all unknown: a scan knows, and everything above it stops knowing.
        Node::Get { catalog, schema, table, .. } => stats.get(&Key::Rows {
            catalog: plan.string(catalog),
            schema: plan.string(schema),
            table: plan.string(table),
        }),
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
            let (kept, from) = kept(plan, input, predicate, stats, reads);
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
            collapsed(plan, input, of(input), keyed(plan, plan.expr_list(groups)), stats, reads)
        }
        // The same shape as a group by on those columns, because that is what it is.
        Node::Distinct { input, on } => {
            let keys = plan.expr_list(on);
            // Plain `DISTINCT` is every column of the row rather than a list of them, so the keys
            // are whatever the input produces. A projection is the input that can be read from
            // here, and it is also the input every `SELECT DISTINCT a, b` has.
            let keys = if keys.is_empty() { produced(plan, input) } else { keyed(plan, keys) };
            collapsed(plan, input, of(input), keys, stats, reads)
        }
        Node::Limit { input, count, offset } => {
            let input = of(input);
            // An offset that is read off the rows while the query runs is a number nobody has
            // here, so nothing is taken away. That is the safe direction, because every answer in
            // this arm is an upper bound and subtracting too little keeps it one.
            let offset = offset.rows().unwrap_or(0);
            match count.rows() {
                // `OFFSET` with no `LIMIT` takes rows away and cannot add any, and taking a known
                // number of rows off a counted one leaves a counted one. A limit read while the
                // query runs is here too, for the same reason the offset above is.
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
        // A share of an unknown number of rows is still unknown, which is the difference from the
        // arm above: a row count is a ceiling whatever feeds it and a percentage is not.
        Node::LimitPercent { input, percent, offset } => of(input).map(|n| {
            // A share or an offset read off the rows is a number nobody has yet, and the widest
            // one it could be is the honest guess, so the estimate stays an upper bound.
            let share = percent.percent().unwrap_or(100.0) / 100.0 * n as f64;
            (share as u64).saturating_sub(offset.rows().unwrap_or(0))
        }),
        Node::TopN { input, count, offset, .. } => match of(input) {
            Stat::Unknown => {
                Stat::Known { value: count, class: CEILING, provenance: FROM_A_CONSTANT }
            }
            known => known.map(|n| n.saturating_sub(offset).min(count)),
        },
        Node::Join { left, right, kind, conditions, .. } => join(
            Both { rows: of(left), base: unfiltered(plan, left, stats) },
            Both { rows: of(right), base: unfiltered(plan, right, stats) },
            kind,
            plan.expr_list(conditions).len(),
            keyspace(plan, conditions, stats, reads),
        ),
        // The one join in the engine whose shape is known before any number is. A forward link
        // answers at most one parent per child row, so the child's count is the answer rather than
        // a number to take a constant fraction of: a left join emits exactly the child's rows, and
        // inner, semi and anti emit the ones whose link is not the no parent sentinel, which is a
        // subset of them. Nothing here guesses a selectivity, and an estimate that does not have to
        // guess is most of why section 6.4 prefers this shape where it can have it.
        Node::LinkJoin { child, kind: JoinKind::Left, .. } => of(child),
        Node::LinkJoin { child, .. } => ceiling(of(child)),
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
/// The product over the conditions, with each condition answered by the best supplier that can
/// answer it and by [`KEPT_BY_A_CONDITION`] where none can.
///
/// `IS NULL` and `IS NOT NULL` are not assumptions either. Every store that keeps bounds per part
/// keeps the null count beside them, so the answer is a count and its complement is the other one.
/// That is [`missing`], and it is asked first because it is the cheapest of the three.
///
/// An equality against a constant on a column the store counted per value is not an assumption at
/// all. The synopsis says how many rows hold that value, or lists every value and does not hold it,
/// and either way the answer is a count. That is [`common`], and it is asked first.
///
/// An equality against a constant on a column somebody counted the distinct values of keeps one
/// value out of however many the column holds. That is the uniformity assumption and is the oldest
/// textbook rule there is, and it is what the line above replaces where it can. One
/// over the count is not always smaller than the constant and is not meant to be: a column of three
/// values gives a third, which is above the fifth the constant guessed, and that is the direction
/// the constant was wrong in for `o_orderstatus`. The rule is to use the number where there is one,
/// not to make the answer smaller.
///
/// A range against a constant is interpolated between the bounds the file already states, per part,
/// by [`spread`]. `l_shipdate <= '1998-09-02'` keeps ninety eight percent of TPC-H's lineitem and
/// the constant called it twenty, and the two ends of the column were sitting in the footer the
/// whole time saying 1992 and 1998.
///
/// The conditions the count did not answer go to the bounds together rather than one at a time, so
/// that two of them on one column are intersected into the interval they name instead of multiplied
/// into a wider one. What is still multiplied is one condition against the next, and both numbers
/// are the column's own taken where the column is read, so a filter above a join reads the base
/// table's facts and applies them to an input something else has already cut down. That is the
/// standard reading and it is why this stays [`GUESSED`]: it assumes the conditions and whatever
/// happened underneath are independent, which is the assumption every estimator makes and the one
/// that fails first.
fn kept(
    plan: &Plan,
    input: NodeRef,
    predicate: ExprRef,
    stats: &Facts,
    reads: &mut Vec<Stat<u64>>,
) -> (f64, Provenance) {
    let mut fraction = 1.0;
    let mut counted = 0;
    let mut source: Option<Provenance> = None;
    let mut pending = Vec::new();
    for conjunct in conjuncts(plan, predicate) {
        match counted_by(plan, input, conjunct, stats, reads) {
            Some((share, from)) => {
                fraction *= share;
                counted += 1;
                source = match source {
                    None => Some(from),
                    Some(one) if one == from => Some(one),
                    Some(_) => Some(Provenance::Propagation),
                };
            }
            None => pending.push(conjunct),
        }
    }
    let interpolated = spread(plan, input, &pending).map_or(0, |spread| {
        fraction *= spread.fraction;
        spread.read
    });
    // Every condition nobody could answer is still worth the constant, and there is one of them per
    // condition rather than one for the lot, because the fifth is a guess about one condition.
    let guessed = pending.len().saturating_sub(interpolated);
    for _ in 0..guessed {
        fraction *= KEPT_BY_A_CONDITION;
    }
    // A fraction two different suppliers contributed to came from the arithmetic over them rather
    // than from either, which is what `Propagation` is for. A reader chasing a bad estimate wants to
    // know which supplier to go and look at without reading the predicate back.
    // A distinct count says where it came from rather than naming one source for all of them. It is
    // a native table's dictionary, or what a Parquet writer counted per row group, or the sketch a
    // table in memory builds as its rows arrive, and a reader chasing a bad estimate wants to know
    // which of the three to go and look at. Two conditions whose counts came from different places
    // say `Propagation` for the same reason a fraction two suppliers contributed to does.
    let from = match (counted, interpolated, guessed) {
        (0, 0, _) => FROM_A_CONSTANT,
        (_, 0, 0) => source.unwrap_or(FROM_A_CONSTANT),
        (0, _, 0) => Provenance::ZoneMap,
        _ => Provenance::Propagation,
    };
    (fraction, from)
}

/// What fraction of a scan's rows these conditions are expected to keep, interpolated between bounds.
///
/// The same reading of the plan [`surviving`] makes, and the same refusals, for the same reasons:
/// straight on a scan whose store kept bounds, positions turned into names before the store is
/// asked, and a name the store does not have gives up rather than guessing.
///
/// What is different is which way the answer can be wrong. [`surviving`] proves parts hold nothing
/// and its answer is a ceiling. This one assumes the values inside a part are spread evenly between
/// its two ends, which is a guess that can land either side of the truth, so it multiplies into the
/// guess rather than capping it.
///
/// Every condition at once, because two of them on one column are one interval and not two, and a
/// store handed them separately has no way to know they belong together. [`Spread::read`] comes back
/// with how many of them the store could read, which is what the caller charges its constant for the
/// rest by.
fn spread(plan: &Plan, input: NodeRef, conjuncts: &[ExprRef]) -> Option<Spread> {
    let (zones, tests) = asked(plan, input, conjuncts)?;
    zones.spread(&tests)
}

/// What fraction of a scan's rows one condition keeps, where somebody counted rather than guessed.
///
/// The three counted answers in the order they are worth asking in. The synopsis first, because
/// where it answers it is a count of the rows that pass and the distinct count is a guess about
/// them. Where it does not, nothing has been spent: it is a lookup in a list the store already has
/// parsed. `None` where none of the three could read the condition, which leaves the caller to
/// decide between the bounds and the constant.
fn counted_by(
    plan: &Plan,
    input: NodeRef,
    conjunct: ExprRef,
    stats: &Facts,
    reads: &mut Vec<Stat<u64>>,
) -> Option<(f64, Provenance)> {
    missing(plan, input, conjunct, reads)
        .or_else(|| common(plan, input, conjunct, stats, reads))
        .or_else(|| values(plan, conjunct, stats, reads).map(|(v, from)| (1.0 / widened(v), from)))
}

/// What fraction of a scan's rows one condition keeps, with the constant where nothing could say.
///
/// [`kept`] asks the same question about a whole predicate and answers with one number for the lot,
/// which is what a cardinality is. A caller ordering the conditions against each other needs them
/// apart, because the whole of what it is deciding is which of two conditions throws more away.
///
/// The bounds are asked for one condition at a time here and for all of them at once there, and the
/// difference is deliberate rather than an oversight. Two range conditions on one column are one
/// interval and a cardinality wants them intersected. An ordering wants to know what each of them
/// does on its own, since they are going to run one after the other and the second one runs on the
/// rows the first one left.
///
/// The provenance comes back beside the fraction so that a caller can tell a measurement from the
/// constant. A condition nobody could read gets [`KEPT_BY_A_CONDITION`] and says
/// [`FROM_A_CONSTANT`], and a caller that reorders on the strength of it would be reordering on the
/// strength of the same number twice.
pub(crate) fn kept_by(
    plan: &Plan,
    input: NodeRef,
    conjunct: ExprRef,
    stats: &Facts,
) -> (f64, Provenance) {
    let mut reads = Vec::new();
    if let Some(answer) = counted_by(plan, input, conjunct, stats, &mut reads) {
        return answer;
    }
    match spread(plan, input, std::slice::from_ref(&conjunct)) {
        Some(spread) if spread.read > 0 => (spread.fraction, Provenance::ZoneMap),
        _ => (KEPT_BY_A_CONDITION, FROM_A_CONSTANT),
    }
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

/// The columns a set of grouping expressions groups by, and `None` where one of them is not one.
///
/// Only a plain column reference. `GROUP BY lower(name)` has as many groups as there are distinct
/// results of the function and nobody counted those, and `GROUP BY a + 1` has as many as `a` has but
/// reading that off would be reasoning about which functions are injective, which is a bigger
/// question than the one being answered. Either of them drops the whole grouping back to the
/// constant rather than being left out of the product, because a key nobody can read is a key that
/// can multiply the groups by any number at all.
pub(crate) fn keyed(plan: &Plan, keys: &[ExprRef]) -> Option<Vec<ColumnBinding>> {
    keys.iter()
        .map(|&key| match *plan.expr(key) {
            Expr::Column(binding) => Some(binding),
            _ => None,
        })
        .collect()
}

/// Every column an operator produces, for a plain `DISTINCT`, which groups by all of them.
///
/// Only over a projection, which is what a `SELECT DISTINCT` of named columns is bound to. Anything
/// else needs the output width of an arbitrary operator, and the one place that is written down is
/// the binder. `None` leaves the grouping on the constant, which is where every one of them was.
fn produced(plan: &Plan, input: NodeRef) -> Option<Vec<ColumnBinding>> {
    let Node::Project { index, exprs, .. } = *plan.node(input) else {
        return None;
    };
    let width = u32::try_from(plan.expr_list(exprs).len()).ok()?;
    Some((0..width).map(|column| ColumnBinding { table: index, column }).collect())
}

/// How many rows a grouping leaves, where somebody counted the values of every column it groups by.
///
/// The operator where shape alone says the least. `GROUP BY user_id` over a log table is close to
/// one row in one and `GROUP BY country` over the same table is a few hundred rows out of any
/// number, and the constant is a tenth for both. Over TPC-H lineitem that tenth calls q1's
/// `GROUP BY l_returnflag, l_linestatus` six hundred thousand rows, where the real answer is four.
/// Everything planned above it is planned against that.
///
/// The counts are there to be read. A column with a stated distinct count says how many groups it
/// can make on its own, several of them multiply, and the product is capped at the rows going in
/// because a grouping cannot produce more rows than it consumes.
///
/// The product assumes the keys are independent of each other, which is the same assumption the
/// joins here make and wrong in the same direction: `GROUP BY nation, region` counts 125 groups
/// where a region is a nation's own and there are 25. Wrong by five is a different thing from wrong
/// by a hundred thousand, and the cap keeps it from ever being wrong by more than the input.
///
/// Rows going in and not the table's rows, so a grouping under a selective filter is not given every
/// group the column can make. That is [`landed_on`], and it is why the table's own row count is read
/// here as well: how much of a column is left is what says how many of its values are still in it.
fn collapsed(
    plan: &Plan,
    node: NodeRef,
    input: Stat<u64>,
    keys: Option<Vec<ColumnBinding>>,
    stats: &Facts,
    reads: &mut Vec<Stat<u64>>,
) -> Stat<u64> {
    let (Some(keys), Stat::Known { value: rows, .. }) = (keys, input) else {
        return guess(input, KEPT_BY_A_GROUP_BY);
    };
    let Some(total) = scanned_rows(plan, node, stats).filter(|&total| total > 0) else {
        return guess(input, KEPT_BY_A_GROUP_BY);
    };
    if keys.is_empty() || rows == 0 {
        return guess(input, KEPT_BY_A_GROUP_BY);
    }
    let mut values: u64 = 1;
    let mut source: Option<Provenance> = None;
    for binding in keys {
        // Recorded before it is read and recorded when it is unknown, for the reason [`values`]
        // gives: a read that found nothing is still a read the planner made.
        let stat = stated(plan, binding, stats);
        reads.push(stat);
        // A column with no values in it is empty or all nulls, and neither makes groups to count.
        let Some(counted) = stat.read(DISTINCT).copied().filter(|&counted| counted > 0) else {
            return guess(input, KEPT_BY_A_GROUP_BY);
        };
        values = values.saturating_mul(counted);
        let from = stat.provenance().unwrap_or(FROM_A_CONSTANT);
        source = match source {
            None => Some(from),
            Some(one) if one == from => Some(one),
            // Two keys counted by different means. The number came from the arithmetic over them
            // rather than from either, which is what `Propagation` is for elsewhere here too.
            Some(_) => Some(Provenance::Propagation),
        };
    }
    let groups = landed_on(values, rows, total);
    guess_from(input, groups as f64 / rows as f64, source.unwrap_or(FROM_A_CONSTANT))
}

/// How many rows the table under an operator holds, for a subtree that reads exactly one.
///
/// Walks down the single input chain to the scan. `None` at a join, a set operation or anything else
/// with two inputs, because two tables have two row counts and what the caller wants is the one the
/// grouping's own columns came out of. `None` at a table function too, which states no count.
///
/// The budget is against a malformed plan rather than a deep one, the same as [`follow`]'s.
fn scanned_rows(plan: &Plan, node: NodeRef, stats: &Facts) -> Option<u64> {
    let mut at = node;
    for _ in 0..16 {
        if matches!(*plan.node(at), Node::Get { .. }) {
            return rows_stat(plan, at, stats).value().copied();
        }
        match plan.node(at).children() {
            [Some(input), None] => at = input,
            _ => return None,
        }
    }
    None
}

/// How many of a column's `values` distinct values are left in `rows` of the `total` it started at.
///
/// Neither of the two obvious answers on its own. A column of three values over six million rows
/// holds all three, and two rows of it hold at most two, so the answer is the values at one end and
/// the rows at the other. Taking the smaller of the two is right at both ends and wrong in the
/// middle, where it says a hundred rows of a hundred value column hold a hundred values, and rows
/// start colliding long before that.
///
/// So this is how many of the values at least one surviving row still holds. A value has `total`
/// over `values` rows on average, each of those rows survived with the probability the whole column
/// did, and a value is gone when every one of them went. That is the whole formula, and the two ends
/// fall out of it: nothing filtered leaves every value, and a filter down to a handful of rows
/// leaves a handful of values.
///
/// It assumes a filter takes rows without regard to what they hold, which a real one does not. A
/// filter on the grouping column itself leaves far fewer groups than this says, and a sorted column
/// under any filter does too. That is above the truth rather than below it, which is the direction
/// the rest of this module is wrong in as well.
#[expect(
    clippy::cast_precision_loss,
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss,
    reason = "a count of groups is a weight here and not an identity"
)]
fn landed_on(values: u64, rows: u64, total: u64) -> u64 {
    let ceiling = values.min(rows).max(1);
    if rows >= total {
        // Nothing was filtered, so every value the column has is still in it. Reading the formula
        // here instead would say a column of a thousand values over its own thousand rows holds six
        // hundred of them, which is a statement about a sample and this is not one.
        return ceiling;
    }
    let kept = rows as f64 / total as f64;
    let each = total as f64 / values as f64;
    let survives = 1.0 - (1.0 - kept).powf(each);
    ((values as f64 * survives).round() as u64).clamp(1, ceiling)
}

/// The columns a scan produces, in the order the plan numbers them.
///
/// `None` unless the filter sits straight on a scan. Straight on, with no projection in between,
/// because a projection renames columns and the name is what a store is asked by, and following one
/// through would be a second place that has to agree with the first about what a column is called.
fn scanned_fields(plan: &Plan, input: NodeRef) -> Option<&[Field]> {
    match *plan.node(input) {
        Node::Get { columns, .. } | Node::TableFunction { columns, .. } => {
            Some(plan.field_list(columns))
        }
        _ => None,
    }
}

/// How many rows of the scan's column at `position` are null, as the store that keeps bounds says.
///
/// The position is the plan's and the name is what the store is asked by, for the reason
/// [`scanned_fields`] gives. `None` for a filter that is not straight on a scan, a store that keeps
/// no bounds, and a column the store does not have.
fn nulls_at(plan: &Plan, input: NodeRef, position: usize) -> Option<Stat<u64>> {
    let index = bounds::scanned(plan, input)?;
    let zones = plan.zones(index)?;
    let name = &scanned_fields(plan, input)?.get(position)?.name;
    Some(zones.nulls(zones.column(name)?))
}

/// A null count that licenses a rewrite is entitled to an exact one and to nothing else.
///
/// [`NULLS`] is the same number read to choose between two plans, where any class will do because a
/// wrong number there is a slow query. Here a wrong number is a wrong answer, so this is
/// [`Use::Enable`] and a count the store estimated does not get through.
pub(crate) const NO_NULLS: Use = Use::Enable;

/// Whether the store says the column this binding names holds no nulls at all.
///
/// The certificate `spec/stats/05-every-query.md` section 5.10 is about. A column with an exact zero
/// null count has a null branch that is provably dead, and the branch is dead at every level: the
/// mask is not written, the kernels do not test it, and a predicate asking whether a value is null
/// has an answer before the query runs.
///
/// Exact and nothing else, through [`NO_NULLS`]. An estimated null count of zero is a column nobody
/// found a null in, which is not the same claim and is the claim that loses rows.
///
/// The count is about a column of a table, and the question here is about a value an operator is
/// reading, so the two are only the same claim if nothing in between put a null there. `input` is
/// what the expression reads from, and [`walk::scan_of`] walks down it to the scan through the
/// operators that keep a value as it was. A left join is not one of them: it pads, and a column that
/// holds no nulls in the file holds one in every row the join had no match for. That is what makes
/// `LEFT JOIN ... WHERE parent.x IS NULL` the way to write an anti join, and settling that predicate
/// on the file's count would answer the opposite of the question.
pub(crate) fn never_null(plan: &Plan, input: NodeRef, binding: ColumnBinding) -> bool {
    let Some(at) = walk::scan_of(plan, input, binding.table) else {
        return false;
    };
    let Some(stat) = nulls_at(plan, at, binding.column as usize) else { return false };
    stat.read(NO_NULLS) == Some(&0)
}

/// One equality or inequality between a column of the scan numbered `index` and a constant.
///
/// Written either way round, because the optimizer does not normalise which side the constant sits
/// on, and an equality reads the same reversed. `None` for anything else, which includes a null
/// constant: [`Bound::of_value`] refuses it, and a comparison against null is null rather than a
/// question about a value.
///
/// [`Bound::of_value`]: rudb_common::bounds::Bound::of_value
fn compared(plan: &Plan, index: u32, conjunct: ExprRef) -> Option<(CompareOp, usize, Bound)> {
    let Expr::Compare { op, left, right } = *plan.expr(conjunct) else {
        return None;
    };
    if !matches!(op, CompareOp::Equal | CompareOp::NotEqual) {
        return None;
    }
    let (binding, value) = match (plan.expr(left), plan.expr(right)) {
        (&Expr::Column(binding), &Expr::Constant(value))
        | (&Expr::Constant(value), &Expr::Column(binding)) => (binding, value),
        _ => return None,
    };
    // The column has to belong to the scan this filter sits on. A binding naming anything else is a
    // column from somewhere below, and the store's counts are not about it.
    if binding.table != index {
        return None;
    }
    Some((op, binding.column as usize, Bound::of_value(plan.value(value))?))
}

/// What fraction of a scan's rows `IS NULL` keeps, where the store counted its nulls.
///
/// The one filter whose answer a store can simply state. Every store that keeps bounds per part
/// keeps the null count beside them, and `IS NULL` used to get the same fifth that any condition
/// nobody could read gets. On a column with no nulls at all that fifth is out by the whole table,
/// and on a mostly null column it is out the other way.
///
/// `IS NOT NULL` is the complement and comes from the same number, which is the reason both are
/// here rather than only the first. A column the store says has no nulls makes `IS NOT NULL` the
/// whole table, and that is worth as much as the other direction: it stops a filter that throws
/// nothing away from being costed as though it threw four fifths away.
///
/// Only against a literal null, which is what `IS NULL` and `IS NOT NULL` bind to. `a IS DISTINCT
/// FROM b` is a comparison of two columns wearing the same operator and the null count says nothing
/// about it.
///
/// `None` where the filter is not straight on a scan, where the store kept no bounds, where it
/// states no null count for the column, and where it says it has no rows. All of those fall back to
/// the constant, which is where this was before.
fn missing(
    plan: &Plan,
    input: NodeRef,
    conjunct: ExprRef,
    reads: &mut Vec<Stat<u64>>,
) -> Option<(f64, Provenance)> {
    let Expr::Compare { op, left, right } = *plan.expr(conjunct) else {
        return None;
    };
    let wants_null = match op {
        CompareOp::NotDistinctFrom => true,
        CompareOp::DistinctFrom => false,
        _ => return None,
    };
    let binding = match (plan.expr(left), plan.expr(right)) {
        (&Expr::Column(binding), &Expr::Constant(value))
        | (&Expr::Constant(value), &Expr::Column(binding))
            if matches!(plan.value(value), Value::Null) =>
        {
            binding
        }
        _ => return None,
    };
    let index = bounds::scanned(plan, input)?;
    if binding.table != index {
        return None;
    }
    let stat = nulls_at(plan, input, binding.column as usize)?;
    // Recorded before it is read and recorded when it is unknown, for the reason [`values`] gives.
    reads.push(stat);
    let nulls = stat.read(NULLS).copied()?;
    // The store's own total and not the plan's, so the fraction is two numbers out of one store.
    // With no tests at all this is every row the store holds, per `Zones::surviving`.
    let rows = plan.zones(index)?.surviving(&[])?;
    if rows == 0 {
        return None;
    }
    // Saturating because a null count above the row count is a store contradicting itself, and the
    // honest reading of that is no rows left rather than a fraction below zero.
    let held = if wants_null { nulls.min(rows) } else { rows.saturating_sub(nulls) };
    Some((share(held, rows), Provenance::NullCount))
}

/// What fraction of a scan's rows a test against a constant keeps, where the store counted values.
///
/// The number the uniformity assumption is guessing at. `o_orderstatus = 'F'` over TPC-H's orders
/// keeps 729,413 rows of 1,500,000, and a column of three values divided by three calls it 500,000.
/// A synopsis that lists all three says the first number, and says it as a count rather than as an
/// estimate, because a complete synopsis accounts for every row of the column.
///
/// Three shapes, all of them the same counts added up differently.
///
/// `c = k` is the count for `k`, or none at all when a complete list does not hold `k`.
///
/// `c <> k` is every row the column has except the ones holding `k` and except the nulls, because
/// `<>` against a null is null and a null row does not pass. So this one needs the null count as
/// well and gives up without it, which costs nothing in practice: a store that counted its values
/// per column kept bounds per part too.
///
/// `c IN (j, k)` binds to an `OR` of equalities and is the counts summed. Only when every branch is
/// an equality on the same column against a constant, because that is the shape an `IN` list makes
/// and anything else is a disjunction this has no arithmetic for. Two branches naming the same value
/// would double count, and the sum is capped at the rows for that reason rather than checked for it.
///
/// A value the synopsis does not list, where the synopsis left something out, is answered from the
/// remainder rather than given up on. See [`unlisted`], which is what makes the two halves of a
/// prefix worth more together than either alone.
///
/// `None` where the scan's store keeps no synopsis, where the constant does not compare against what
/// the synopsis holds, where the remainder needs a distinct count nobody stated, and where the store
/// says it has no rows at all. Every one of those falls through to the distinct count, which is where
/// the estimate was before this existed.
fn common(
    plan: &Plan,
    input: NodeRef,
    conjunct: ExprRef,
    stats: &Facts,
    reads: &mut Vec<Stat<u64>>,
) -> Option<(f64, Provenance)> {
    let index = bounds::scanned(plan, input)?;
    let frequencies = plan.frequencies(index)?;
    let fields = scanned_fields(plan, input)?;
    let rows = frequencies.rows();
    if rows == 0 {
        return None;
    }
    // Set by the first value answered out of the remainder rather than out of the list, because that
    // answer is two stores put together and the provenance printed for it should say so.
    let mut blended = false;
    // One read of the synopsis per value asked about, recorded whether it answered or not for the
    // reason [`values`] gives: the misses are the interesting half of what `EXPLAIN (STATISTICS)`
    // prints.
    let mut counted = |position: usize, value: &Bound| {
        let column = frequencies.column(&fields.get(position)?.name)?;
        let stat = frequencies.rows_with(column, value);
        reads.push(stat);
        if let Some(count) = stat.read(COMMON).copied() {
            return Some(count);
        }
        let binding = ColumnBinding { table: index, column: u32::try_from(position).ok()? };
        let held = unlisted(plan, frequencies, column, binding, stats, reads)?;
        blended = true;
        Some(held)
    };
    let held = match *plan.expr(conjunct) {
        Expr::Conjunction { op: ConjunctionOp::Or, children } => {
            let branches = plan.expr_list(children);
            // Capped for the reason [`conjuncts`] caps: a list written by a generator can be
            // thousands long, each branch is a walk of the synopsis, and past a handful the sum has
            // stopped telling anybody anything the whole column's count would not.
            if branches.is_empty() || branches.len() > 32 {
                return None;
            }
            let mut column = None;
            let mut total: u64 = 0;
            for &branch in branches {
                let (CompareOp::Equal, position, value) = compared(plan, index, branch)? else {
                    return None;
                };
                // Every branch on one column. Two columns is a disjunction over two things and
                // adding their counts would be arithmetic about neither.
                if *column.get_or_insert(position) != position {
                    return None;
                }
                total = total.checked_add(counted(position, &value)?)?;
            }
            total.min(rows)
        }
        _ => {
            let (op, position, value) = compared(plan, index, conjunct)?;
            let counted = counted(position, &value)?;
            match op {
                CompareOp::Equal => counted,
                // Not the complement of the count but the complement inside the rows that are not
                // null, since `c <> k` is null for a null row and a null row does not pass. The
                // null count is the other store's and the total is this one's, so both subtractions
                // saturate: on one table they are the same reader, and two readers disagreeing
                // about the rows is no reason to report a filter that grows its input.
                _ => {
                    let nulls = nulls_at(plan, input, position)?.read(NULLS).copied()?;
                    rows.saturating_sub(nulls).saturating_sub(counted)
                }
            }
        }
    };
    let from = if blended { Provenance::Propagation } else { Provenance::FrequencySynopsis };
    Some((share(held, rows), from))
}

/// How many rows hold a value the synopsis lists nowhere, where it lists only the leading values.
///
/// The remainder half of a prefix, and the last piece of the synopsis worth reading. The list holds
/// the values that take the most rows and says exactly how many, so subtracting them leaves the rows
/// of the tail, and subtracting the values it listed from the column's distinct count leaves how many
/// values those rows are shared between. One divided by the other is the uniformity assumption asked
/// of the tail alone, which is the part of the column it was ever true of.
///
/// The difference is the whole of what a prefix is for. A column with one value in half a million of
/// its million rows and a long tail behind it never gets a complete synopsis. Dividing the million
/// rows by the thousand values calls every tail value a thousand rows, and the tail really holds five
/// hundred thousand rows over nine hundred and ninety nine values, which is five hundred.
///
/// Capped at the bound the writer recorded, which is a real ceiling rather than a second guess: the
/// heavy hitter pass proved no value it dropped holds more rows than that. Floored at one because a
/// value being asked about is a value, and a division that rounds to nothing would say a row that
/// exists does not.
///
/// Only an exact distinct count divides. Where the number of values is a ceiling rather than a count,
/// which is what a native file's integer column gets out of the span between its two ends, the
/// division is over more values than the column has and comes out under the truth by that factor. A
/// native column of a thousand values between 1 and 1999 gets 1999 from the span, and its tail of
/// 4,890 rows divided by the 1,487 values that ceiling leaves is three rows where the answer is ten.
/// So without a count the answer is the writer's bound on its own, which is where the tail sits when
/// the tail is flat, and is the direction that over-counts rather than under-counts a filter.
///
/// `None` only where the store has no remainder to spread, which is a complete list or no synopsis at
/// all. A distinct count below what the synopsis listed is two reads of one column contradicting each
/// other, and that falls back to the bound rather than to arithmetic on the disagreement.
fn unlisted(
    plan: &Plan,
    frequencies: &Arc<dyn Frequencies>,
    column: usize,
    binding: ColumnBinding,
    stats: &Facts,
    reads: &mut Vec<Stat<u64>>,
) -> Option<u64> {
    let remainder = frequencies.remainder(column)?;
    // Recorded before it is read and recorded when it is unknown, for the reason [`values`] gives.
    let stat = stated(plan, binding, stats);
    reads.push(stat);
    let counted = match stat {
        Stat::Known { value, class: Class::Exact, .. } => Some(value),
        _ => None,
    };
    let divided = counted
        .and_then(|values| values.checked_sub(remainder.listed))
        .filter(|&rest| rest > 0)
        .map(|rest| remainder.rows / rest);
    Some(divided.unwrap_or(remainder.most).clamp(1, remainder.most.max(1)))
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
fn values(
    plan: &Plan,
    conjunct: ExprRef,
    stats: &Facts,
    reads: &mut Vec<Stat<u64>>,
) -> Option<(u64, Provenance)> {
    let Expr::Compare { op: CompareOp::Equal, left, right } = *plan.expr(conjunct) else {
        return None;
    };
    let binding = match (plan.expr(left), plan.expr(right)) {
        (&Expr::Column(binding), _) if walk::constant(plan, right) => binding,
        (_, &Expr::Column(binding)) if walk::constant(plan, left) => binding,
        _ => return None,
    };
    // Recorded before it is read, and recorded even when it turns out to be unknown. A read that
    // found nothing is still a read the planner made, and the section `EXPLAIN (STATISTICS)` prints
    // is worth far less if the misses are left out of it.
    let stat = stated(plan, binding, stats);
    reads.push(stat);
    // A column with no values in it is an empty column or a column of nothing but nulls, and
    // neither is something to divide by.
    let values = stat.read(DISTINCT).copied().filter(|&values| values > 0)?;
    // A known stat always has a provenance, so the fallback is for a shape that cannot occur and
    // not for a case anybody has to read.
    Some((values, stat.provenance().unwrap_or(FROM_A_CONSTANT)))
}

/// How many rows sit in the parts of `input` that `predicate` cannot rule out.
///
/// A ceiling and not an estimate, which is why the caller takes the smaller of this and its guess
/// rather than multiplying the two together.
fn surviving(plan: &Plan, input: NodeRef, predicate: ExprRef) -> Option<u64> {
    let (zones, tests) = asked(plan, input, &[predicate])?;
    zones.surviving(&tests)
}

/// The store under `input` and the tests `predicates` read as, named the way that store names them.
///
/// `None` unless the filter sits straight on a scan whose store kept bounds and at least one
/// conjunct reads as a test. Straight on, with no projection in between, because a projection
/// renames columns and the name is what the store is asked by, and following one through would be a
/// second place that has to agree with the first about what a column is called.
///
/// A conjunct that is not a test is not a refusal. Dropping it leaves parts in that a full reading
/// would have ruled out, which is the safe direction for both callers: it leaves [`surviving`] with
/// a looser ceiling and [`spread`] with a larger fraction, and the guess above either still applies.
/// What is a refusal is a test naming a column the store does not have, which means this plan and
/// this store disagree about what is being read, and a number worked out from that disagreement
/// would rule out parts holding rows the query wants.
///
/// The position is turned into a name and the name is given to the store, rather than the position
/// being handed over directly. Column pruning moves a scan's positions and moves nothing else, so a
/// position is about the plan and the store numbers its columns the way the file does.
fn asked<'a>(
    plan: &'a Plan,
    input: NodeRef,
    predicates: &[ExprRef],
) -> Option<(&'a Arc<dyn Zones>, Vec<Test>)> {
    let index = bounds::scanned(plan, input)?;
    let zones = plan.zones(index)?;
    let names = match *plan.node(input) {
        Node::Get { columns, .. } | Node::TableFunction { columns, .. } => plan.field_list(columns),
        _ => return None,
    };
    let mut tests = Vec::new();
    for predicate in predicates {
        for (position, op, value) in bounds::of(plan, input, *predicate) {
            let name = &names.get(position)?.name;
            tests.push(Test { column: zones.column(name)?, op, value });
        }
    }
    (!tests.is_empty()).then_some((zones, tests))
}

/// How many distinct values the column a binding names holds, where anybody counted.
///
/// A binding names the operator that produces the column and the position it has there, so this
/// finds the operator and asks what the column at that position is called. Only a scan has an
/// answer, and a projection is followed through to one.
///
/// A column nobody counted falls back to the table's rows. See [`Missing::Rows`] for why, and
/// [`stated`] for the caller that cannot take that answer.
fn distinct(plan: &Plan, binding: ColumnBinding, stats: &Facts) -> Stat<u64> {
    follow(plan, binding, stats, Missing::Rows, 16)
}

/// [`distinct`] restricted to columns somebody actually counted.
pub(crate) fn stated(plan: &Plan, binding: ColumnBinding, stats: &Facts) -> Stat<u64> {
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
    ///
    /// It comes back through [`ceiling`] so that the class says which of the two it is. The row
    /// count itself is exact and the distinct count it stands in for is not, and handing the
    /// catalog's [`Class::Exact`] straight over would tell a reader the column was counted when
    /// nobody counted it.
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
    stats: &Facts,
    missing: Missing,
    depth: u32,
) -> Stat<u64> {
    let Some(depth) = depth.checked_sub(1) else {
        return Stat::Unknown;
    };
    let position = binding.column as usize;
    let rows = missing == Missing::Rows;
    for at in 0..u32::try_from(plan.node_count()).unwrap_or(u32::MAX) {
        match *plan.node(at) {
            Node::Get { catalog, schema, table, index, columns, .. } if index == binding.table => {
                let Some(field) = plan.field_list(columns).get(position) else {
                    return Stat::Unknown;
                };
                let catalog = plan.string(catalog);
                let schema = plan.string(schema);
                let table = plan.string(table);
                let distinct =
                    stats.get(&Key::Distinct { catalog, schema, table, column: &field.name });
                if matches!(distinct, Stat::Known { .. }) {
                    return distinct;
                }
                // What the store said about itself, which for a native table is the dictionary for
                // a string column and the span between the two ends for an integer one. Second to
                // `ANALYZE`, because `ANALYZE` counted the column and this bounds it.
                let measured = plan.distinct_measured(index, &field.name);
                if matches!(measured, Stat::Known { .. }) {
                    return measured;
                }
                if !rows {
                    return Stat::Unknown;
                }
                return ceiling(stats.get(&Key::Rows { catalog, schema, table }));
            }
            Node::TableFunction { index, columns, .. } if index == binding.table => {
                let Some(field) = plan.field_list(columns).get(position) else {
                    return Stat::Unknown;
                };
                let distinct = plan.distinct_measured(index, &field.name);
                if matches!(distinct, Stat::Known { .. }) {
                    return distinct;
                }
                return if rows { ceiling(plan.measured(index)) } else { Stat::Unknown };
            }
            Node::Project { index, exprs, .. } if index == binding.table => {
                let Some(&carried) = plan.expr_list(exprs).get(position) else {
                    return Stat::Unknown;
                };
                let &Expr::Column(carried) = plan.expr(carried) else {
                    return Stat::Unknown;
                };
                return follow(plan, carried, stats, missing, depth);
            }
            _ => {}
        }
    }
    Stat::Unknown
}

/// How many pairs of values the conditions of a join can match on, where every one is understood.
///
/// The product over the conditions of the larger of the two sides' distinct counts, which is the
/// standard reading of an equijoin: the two columns draw from a shared set of values, the larger
/// count is how big that set is, and the values are assumed to be spread evenly over it. `None`
/// unless every condition is an equality between two base columns that both have a count, because a
/// condition nobody understood could be the one doing all the work and a divisor that left it out
/// would claim more rows than the join can produce.
fn keyspace(
    plan: &Plan,
    conditions: Slice,
    stats: &Facts,
    reads: &mut Vec<Stat<u64>>,
) -> Option<u64> {
    keyspace_into(plan, plan.expr_list(conditions), stats, reads)
}

/// `keyspace` over conditions the caller is holding rather than over a slice of the arena.
///
/// Join ordering wants this. It is deciding which pair to join next and the conditions that would
/// apply at that pair are the ones it has just worked out are testable there, which is a list it
/// built and not a list any node in the arena holds. It is not counting its reads, because the
/// orders it scores are candidates and most of them are thrown away, and a histogram that counted
/// every discarded candidate would say more about the search than about the plan.
#[must_use]
pub fn keyspace_of(plan: &Plan, conditions: &[ExprRef], stats: &Facts) -> Option<u64> {
    keyspace_into(plan, conditions, stats, &mut Vec::new())
}

/// [`keyspace_of`] with the distinct counts it read collected as it goes.
fn keyspace_into(
    plan: &Plan,
    conditions: &[ExprRef],
    stats: &Facts,
    reads: &mut Vec<Stat<u64>>,
) -> Option<u64> {
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
        // Both sides are recorded before either is read, so that a condition one side of which
        // nobody counted still shows up as two reads rather than one.
        let (left, right) = (distinct(plan, left, stats), distinct(plan, right, stats));
        reads.push(left);
        reads.push(right);
        let pair = (*left.read(DISTINCT)?).max(*right.read(DISTINCT)?);
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

/// One side of a join, as how many rows it produces and how many it would produce unfiltered.
///
/// The two are the same number on a side nothing filters, which is most sides, and then everything
/// below behaves exactly as [`matched`] alone did.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Side {
    /// The rows the side produces.
    pub rows: u64,
    /// The rows it would produce with the filters under it taken out. Never below `rows`.
    pub base: u64,
}

impl Side {
    /// A side nothing under it filters, which is what a caller with no second number to give says.
    #[must_use]
    pub const fn whole(rows: u64) -> Self {
        Self { rows, base: rows }
    }

    /// What fraction of its rows the filters underneath left, which is one where there are none.
    fn share(self) -> f64 {
        (widened(self.rows) / widened(self.base)).min(1.0)
    }
}

/// How many rows an equijoin of these two sides produces, and how many it would produce unfiltered.
///
/// [`matched`] over what the two sides would be without their filters, times what each filter kept.
/// Splitting it that way is the only reading of the containment assumption that survives a filter.
/// Containment says every value of the smaller side turns up on the larger one, which is a claim
/// about two whole tables, and the rows a filter took off one side are exactly the rows the other
/// side no longer has a partner for. So the assumption is applied where it holds, between the two
/// tables as they stand in the catalog, and the filters are applied to what it produced.
///
/// The fractions multiply, which assumes the two filters are independent of each other and of the
/// join, and that is the same assumption everything else in this module already makes.
///
/// The `base` that comes back is the unfiltered join and not the answer, so that a caller building
/// up an order pair by pair can ask the same question of the pair it just made. Scaling an already
/// scaled number would charge a filter twice, once at the join that first saw it and once at every
/// join above.
#[must_use]
pub fn matched_sides(left: Side, right: Side, keys: Option<u64>) -> Side {
    let base = matched(left.base, right.base, keys);
    let rows = scale(base, left.share() * right.share()).max(1).min(base);
    Side { rows, base }
}

/// A side as the two numbers [`join`] reads: what it produces and what it would produce unfiltered.
#[derive(Clone, Copy)]
struct Both {
    rows: Stat<u64>,
    base: Stat<u64>,
}

/// A side whose second number is its first, which is a side with no filter under it to account for.
const fn both(stat: Stat<u64>) -> Both {
    Both { rows: stat, base: stat }
}

/// The join kinds, each of which is a different question.
fn join(
    left: Both,
    right: Both,
    kind: JoinKind,
    conditions: usize,
    keys: Option<u64>,
) -> Stat<u64> {
    let (left, right, bases) = (left.rows, right.rows, (left.base, right.base));
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
            // The unfiltered size of a side falls back to its filtered one, which makes the
            // fraction one and leaves the estimate where it was before there were two numbers.
            let sides = (
                Side { rows: left, base: bases.0.value().copied().unwrap_or(left).max(left) },
                Side { rows: right, base: bases.1.value().copied().unwrap_or(right).max(right) },
            );
            let matched = matched_sides(sides.0, sides.1, keys).rows;
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

/// What share of `rows` the rows holding one value are, as a fraction nothing can push outside zero
/// to one.
///
/// Clamped because the two numbers come from the same store but not necessarily from the same
/// moment, and a fraction above one would make a filter grow its input, which is a shape the rest
/// of the estimator does not expect from a conjunct.
#[expect(clippy::cast_precision_loss, reason = "a row count is a weight here and not an identity")]
fn share(counted: u64, rows: u64) -> f64 {
    (counted as f64 / rows as f64).clamp(0.0, 1.0)
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

    use rudb_common::bounds::{Bound, End, Frequencies, Op, Remainder, Spread, Test, Zones};
    use rudb_common::stat::{Class, Direction, Provenance, Stat};
    use rudb_plan::Plan;

    use super::{Facts, Key, Side, matched_sides, rows, rows_stat, unfiltered};

    /// A one column scan of the named table, which is what most of these sit on.
    fn scan(table: &str, index: u32) -> String {
        format!("Get memory.main.{table} AS {table} #{index} [a::INTEGER]\n")
    }

    /// The tables named here, sized as given, and nothing else measured.
    fn facts(tables: &[(&str, u64)]) -> Facts {
        let mut stats = Facts::new();
        for (table, count) in tables {
            stats.record("memory", "main", table, *count);
        }
        stats
    }

    /// The estimate for the root of a plan written as text, against the given table sizes.
    fn estimate(text: &str, tables: &[(&str, u64)]) -> Option<u64> {
        let stats = facts(tables);
        let plan =
            Plan::parse(text).unwrap_or_else(|error| panic!("{text} did not parse: {error}"));
        rows(&plan, plan.root(), &stats)
    }

    /// The same estimate with the class still attached.
    fn stat(text: &str, tables: &[(&str, u64)]) -> Stat<u64> {
        let stats = facts(tables);
        let plan =
            Plan::parse(text).unwrap_or_else(|error| panic!("{text} did not parse: {error}"));
        rows_stat(&plan, plan.root(), &stats)
    }

    /// The estimate against table sizes and distinct counts, the counts named table then column.
    fn counted(text: &str, tables: &[(&str, u64)], columns: &[(&str, &str, u64)]) -> Option<u64> {
        let mut stats = facts(tables);
        for (table, column, distinct) in columns {
            stats.record_distinct(
                "memory",
                "main",
                table,
                column,
                *distinct,
                Provenance::Dictionary,
            );
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
        let mut stats = facts(tables);
        for (table, column, distinct) in columns {
            stats.record_distinct(
                "memory",
                "main",
                table,
                column,
                *distinct,
                Provenance::Dictionary,
            );
        }
        let plan =
            Plan::parse(text).unwrap_or_else(|error| panic!("{text} did not parse: {error}"));
        rows_stat(&plan, plan.root(), &stats)
    }

    /// What the root would produce with the filters under it taken out.
    fn whole(text: &str, tables: &[(&str, u64)]) -> Option<u64> {
        let stats = facts(tables);
        let plan =
            Plan::parse(text).unwrap_or_else(|error| panic!("{text} did not parse: {error}"));
        unfiltered(&plan, plan.root(), &stats).value().copied()
    }

    /// The same as [`counted_stat`], with each count carrying the provenance it was recorded
    /// under rather than all of them carrying one.
    fn sourced_stat(
        text: &str,
        tables: &[(&str, u64)],
        columns: &[(&str, &str, u64, Provenance)],
    ) -> Stat<u64> {
        let mut stats = facts(tables);
        for (table, column, distinct, provenance) in columns {
            stats.record_distinct("memory", "main", table, column, *distinct, *provenance);
        }
        let plan =
            Plan::parse(text).unwrap_or_else(|error| panic!("{text} did not parse: {error}"));
        rows_stat(&plan, plan.root(), &stats)
    }

    /// A filter of the given predicate over a two column scan of `t`.
    fn filtered(predicate: &str) -> String {
        format!("Filter {predicate}\n  Get memory.main.t AS t #0 [a::INTEGER, b::INTEGER]\n")
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
        // The one answer here that is a fact rather than a guess, and it holds with no facts
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

    /// A scan of two integer columns, for the tests that group by more than one of them.
    fn wide_scan(table: &str, index: u32) -> String {
        format!("Get memory.main.{table} AS {table} #{index} [a::INTEGER, b::INTEGER]\n")
    }

    #[test]
    fn a_group_by_on_a_counted_column_produces_as_many_groups_as_the_column_has_values() {
        // The constant said a tenth, which over a thousand rows is a hundred groups whether the
        // column holds twenty five values or a million. It holds twenty five and the catalog said so.
        let text = format!("Aggregate #1 groups=[#0.0::INTEGER] aggregates=[]\n  {}", scan("t", 0));
        assert_eq!(
            counted_stat(&text, &[("t", 1000)], &[("t", "a", 25)]),
            Stat::estimated(25, Provenance::Dictionary)
        );
        // Nothing was filtered, so every value of the column is still in it and the count is the
        // answer rather than a share of it.
        assert_eq!(counted(&text, &[("t", 1000)], &[("t", "a", 1000)]), Some(1000));
    }

    #[test]
    fn two_group_keys_multiply_and_the_rows_going_in_cap_the_product() {
        // The independence assumption, which is the one the joins here make as well. Twenty five
        // values against five is a hundred and twenty five pairs where the two are unrelated, and
        // fewer where they are not.
        let text = format!(
            "Aggregate #1 groups=[#0.0::INTEGER, #0.1::INTEGER] aggregates=[]\n  {}",
            wide_scan("t", 0)
        );
        let keys = [("t", "a", 25), ("t", "b", 5)];
        assert_eq!(counted(&text, &[("t", 100_000)], &keys), Some(125));
        // And a grouping cannot produce more rows than it reads, whatever the product says.
        assert_eq!(counted(&text, &[("t", 50)], &keys), Some(50));
    }

    #[test]
    fn a_group_by_under_a_filter_gets_the_values_the_filter_left_rather_than_all_of_them() {
        // The middle, which is where taking the smaller of the values and the rows goes wrong. A
        // hundred values over a thousand rows is ten rows each, a filter down to two hundred rows
        // keeps a fifth of them, and a value is gone only when all ten of its rows went, which
        // happens to about a ninth of the values. So eighty nine groups: under the hundred the
        // column holds and under the two hundred rows going in, and neither of those on its own.
        let text = format!(
            "Aggregate #1 groups=[#0.0::INTEGER] aggregates=[]\n  Filter (#0.0::INTEGER > \
             1::INTEGER)::BOOLEAN\n    {}",
            scan("t", 0)
        );
        assert_eq!(counted(&text, &[("t", 1000)], &[("t", "a", 100)]), Some(89));
    }

    #[test]
    fn a_group_by_on_a_column_nobody_counted_is_the_constant_it_always_was() {
        let text = format!("Aggregate #1 groups=[#0.0::INTEGER] aggregates=[]\n  {}", scan("t", 0));
        assert_eq!(
            counted_stat(&text, &[("t", 1000)], &[]),
            Stat::estimated(100, Provenance::Default)
        );
    }

    #[test]
    fn a_plain_distinct_groups_by_every_column_the_projection_under_it_produces() {
        // `SELECT DISTINCT a, b` states no keys at all, so the keys are the row, and the row is
        // whatever the projection emits. Reading it any other way leaves the commonest way anybody
        // writes a grouping on the constant.
        let text = format!(
            "Distinct on=[]\n  Project #1 [#0.0::INTEGER AS a, #0.1::INTEGER AS b]\n    {}",
            wide_scan("t", 0)
        );
        let keys = [("t", "a", 25), ("t", "b", 5)];
        assert_eq!(counted(&text, &[("t", 100_000)], &keys), Some(125));
    }

    #[test]
    fn a_distinct_on_some_columns_reads_the_columns_it_names() {
        let text = format!(
            "Distinct on=[#1.0::INTEGER]\n  Project #1 [#0.0::INTEGER AS a, #0.1::INTEGER AS \
             b]\n    {}",
            wide_scan("t", 0)
        );
        let keys = [("t", "a", 25), ("t", "b", 5)];
        assert_eq!(counted(&text, &[("t", 100_000)], &keys), Some(25));
    }

    #[test]
    fn a_group_by_over_a_join_keeps_the_constant_because_two_tables_have_two_row_counts() {
        // The counts are per column of a table and the arithmetic here needs the rows that column
        // started at. Over a join there are two of those and the grouping's columns can come from
        // either, so this is a question about a shape rather than a number to be careful with.
        let text = format!(
            "Aggregate #1 groups=[#0.0::INTEGER] aggregates=[]\n  Join Inner on=[]\n    {}    {}",
            scan("t", 0),
            scan("u", 1)
        );
        assert_eq!(
            counted_stat(&text, &[("t", 1000), ("u", 1000)], &[("t", "a", 25)]).provenance(),
            Some(Provenance::Default)
        );
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
            rows_stat(&plan, plan.root(), &Facts::new()),
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
            rows_stat(&plan, plan.root(), &Facts::new()),
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
        assert_eq!(rows_stat(&plan, plan.root(), &Facts::new()), Stat::Unknown);
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
        assert_eq!(rows_stat(&plan, plan.root(), &Facts::new()), Stat::Unknown);
        plan.measure(0, Stat::exact(1000, Provenance::RowCount));
        let over = rows_stat(&plan, plan.root(), &Facts::new());
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
        assert_eq!(rows_stat(&plan, plan.root(), &Facts::new()), Stat::Unknown);
    }

    #[test]
    fn facts_that_nobody_filled_in_say_so() {
        let mut stats = Facts::new();
        assert!(stats.is_empty());
        stats.record("memory", "main", "t", 7);
        assert!(!stats.is_empty());
        assert_eq!(
            stats.get(&Key::Rows { catalog: "memory", schema: "main", table: "t" }),
            Stat::exact(7, Provenance::RowCount)
        );
        // The three names are one key. A table of the same name in another schema is another table,
        // and what comes back for it is unknown rather than a zero, because a table nobody measured
        // and an empty table are not the same thing.
        assert_eq!(
            stats.get(&Key::Rows { catalog: "memory", schema: "other", table: "t" }),
            Stat::Unknown
        );
    }

    #[test]
    fn a_distinct_count_says_where_it_came_from_and_a_missing_one_says_nothing() {
        let mut stats = Facts::new();
        stats.record_distinct("memory", "main", "t", "a", 25, Provenance::Dictionary);
        let key = |column| Key::Distinct { catalog: "memory", schema: "main", table: "t", column };
        assert_eq!(stats.get(&key("a")), Stat::exact(25, Provenance::Dictionary));
        assert_eq!(stats.get(&key("b")), Stat::Unknown, "a column nobody counted");
    }

    #[test]
    fn a_set_built_by_hand_can_never_be_mistaken_for_a_catalog_that_is_current() {
        // A catalog counts from one, so a set nobody read one for answers a generation no catalog
        // ever has. That is what stops a set assembled in a test from being reused as though it
        // were the live one.
        assert_eq!(Facts::new().generation(), 0);
        assert_eq!(Facts::at(12).generation(), 12);
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

    /// A scan of `lineitem` joined to a scan of `part` with an equality filter on the part side.
    fn joined_to_a_filtered_part(filter: bool) -> String {
        let part = if filter {
            concat!(
                "  Filter (#1.0::INTEGER = 3::INTEGER)::BOOLEAN\n",
                "    Get memory.main.part AS part #1 [a::INTEGER]\n"
            )
        } else {
            "  Get memory.main.part AS part #1 [a::INTEGER]\n"
        };
        format!(
            "Join INNER on=[(#0.0::INTEGER = #1.0::INTEGER)::BOOLEAN]\n{}{part}",
            "  Get memory.main.lineitem AS lineitem #0 [a::INTEGER]\n"
        )
    }

    #[test]
    fn the_unfiltered_size_of_a_node_is_the_rows_the_filters_under_it_were_given() {
        // The number containment is a statement about, which is the table rather than the part of
        // it a filter kept. A node with nothing filtering under it reports the same either way.
        let tables = &[("lineitem", 6_000_000), ("part", 200_000)];
        let text = joined_to_a_filtered_part(true);
        assert_eq!(estimate(&text, tables), Some(1_200_000));
        assert_eq!(whole(&text, tables), Some(6_000_000));
        let plain = joined_to_a_filtered_part(false);
        assert_eq!(estimate(&plain, tables), whole(&plain, tables));
    }

    #[test]
    fn a_filter_under_one_side_makes_the_join_smaller_than_the_side_it_contains() {
        // TPC-H q9 written small. The containment floor calls a join to a fiftieth of part the
        // whole of lineitem, which is what made the ordering pass run this join last instead of
        // first. A fiftieth of the parts match a fiftieth of the rows.
        let tables = &[("lineitem", 6_000_000), ("part", 200_000)];
        assert_eq!(estimate(&joined_to_a_filtered_part(false), tables), Some(6_000_000));
        assert_eq!(estimate(&joined_to_a_filtered_part(true), tables), Some(1_200_000));
    }

    #[test]
    fn a_filter_is_charged_once_however_many_joins_sit_above_it() {
        // The unfiltered size that comes back from a join is the unfiltered join, so the join
        // above divides by the same fraction the join below already divided by. Charging it twice
        // would call this two hundred and forty thousand rows and put a supplier join first again.
        let text = concat!(
            "Join INNER on=[(#0.0::INTEGER = #2.0::INTEGER)::BOOLEAN]\n",
            "  Join INNER on=[(#0.0::INTEGER = #1.0::INTEGER)::BOOLEAN]\n",
            "    Get memory.main.lineitem AS lineitem #0 [a::INTEGER]\n",
            "    Filter (#1.0::INTEGER = 3::INTEGER)::BOOLEAN\n",
            "      Get memory.main.part AS part #1 [a::INTEGER]\n",
            "  Get memory.main.supplier AS supplier #2 [a::INTEGER]\n"
        );
        let tables = &[("lineitem", 6_000_000), ("part", 200_000), ("supplier", 10_000)];
        assert_eq!(estimate(text, tables), Some(1_200_000));
        assert_eq!(whole(text, tables), Some(6_000_000));
    }

    #[test]
    fn a_semi_join_is_walked_through_the_way_a_filter_over_the_same_side_is() {
        // The two spellings of the same side, one before the semi passes have run over it and one
        // after. They have to answer the same, because the join ordering pass reads this and the
        // sequence of passes is required to settle after one run over the plan it produced.
        let tables = &[("t", 1_000), ("u", 100)];
        let before = filtered("(#0.0::INTEGER = 3::INTEGER)::BOOLEAN");
        let after = concat!(
            "Join SEMI on=[(#1.0::INTEGER = #0.0::INTEGER)::BOOLEAN]\n",
            "  Filter (#0.0::INTEGER = 3::INTEGER)::BOOLEAN\n",
            "    Get memory.main.t AS t #0 [a::INTEGER, b::INTEGER]\n",
            "  Get memory.main.u AS u #1 [a::INTEGER]\n"
        );
        assert_eq!(whole(&before, tables), Some(1_000));
        assert_eq!(whole(after, tables), Some(1_000));
    }

    #[test]
    fn a_side_with_nothing_under_it_is_whole_and_the_arithmetic_is_the_containment_reading() {
        // The two numbers agree on a side nobody filtered, and then the scaled reading is the
        // unscaled one. This is the case every plan was in before there were two numbers.
        let sides = matched_sides(Side::whole(1_500_000), Side::whole(150_000), Some(150_000));
        assert_eq!(sides, Side { rows: 1_500_000, base: 1_500_000 });
        // A side cut to a tenth takes the join to a tenth, and the base it reports is still the
        // join of the two whole tables.
        let cut = Side { rows: 15_000, base: 150_000 };
        assert_eq!(
            matched_sides(Side::whole(1_500_000), cut, Some(150_000)),
            Side { rows: 150_000, base: 1_500_000 }
        );
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
            Some(Provenance::Dictionary)
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
    fn a_filter_says_which_of_the_three_places_its_count_came_from() {
        // A distinct count is a native table's dictionary, what a Parquet writer counted per row
        // group, or the sketch a table in memory builds as its rows arrive. The estimate names the
        // one it read rather than naming the same one every time, because a reader chasing a bad
        // estimate has to know which of the three to go and look at.
        let tables = &[("t", 1_000_000)];
        let one = filtered("(#0.0::INTEGER = 3::INTEGER)::BOOLEAN");
        assert_eq!(
            sourced_stat(&one, tables, &[("t", "a", 50, Provenance::Sketch)]).provenance(),
            Some(Provenance::Sketch)
        );
        assert_eq!(
            sourced_stat(&one, tables, &[("t", "a", 50, Provenance::Dictionary)]).provenance(),
            Some(Provenance::Dictionary)
        );
        let both = filtered(
            "((#0.0::INTEGER = 3::INTEGER)::BOOLEAN AND (#0.1::INTEGER = 4::INTEGER)::BOOLEAN)::BOOLEAN",
        );
        // Two counts from two places is neither of them.
        assert_eq!(
            sourced_stat(
                &both,
                tables,
                &[("t", "a", 50, Provenance::Sketch), ("t", "b", 40, Provenance::Dictionary)]
            )
            .provenance(),
            Some(Provenance::Propagation)
        );
        // Two counts from the same place is that place.
        assert_eq!(
            sourced_stat(
                &both,
                tables,
                &[("t", "a", 50, Provenance::Sketch), ("t", "b", 40, Provenance::Sketch)]
            )
            .provenance(),
            Some(Provenance::Sketch)
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
        /// What [`Zones::spread`] answers, whatever it is asked.
        spread: Option<f64>,
        /// Every test it was asked, so a test can check which column the estimator named.
        asked: Mutex<Vec<Test>>,
        /// What [`Zones::nulls`] answers, whatever column it is asked about.
        nulls: Stat<u64>,
    }

    impl Stub {
        /// A store that answers the ceiling and refuses to interpolate, which is most of these.
        fn new(surviving: Option<u64>) -> Arc<Self> {
            Arc::new(Self {
                surviving,
                spread: None,
                asked: Mutex::new(Vec::new()),
                nulls: Stat::Unknown,
            })
        }

        /// A store that interpolates and states no ceiling, which is the range case.
        fn spreading(spread: f64) -> Arc<Self> {
            Arc::new(Self {
                surviving: None,
                spread: Some(spread),
                asked: Mutex::new(Vec::new()),
                nulls: Stat::Unknown,
            })
        }

        /// A store of `rows` rows that counted `nulls` of them null, which is the null count case.
        fn counting(rows: u64, nulls: u64) -> Arc<Self> {
            Arc::new(Self {
                surviving: Some(rows),
                spread: None,
                asked: Mutex::new(Vec::new()),
                nulls: Stat::exact(nulls, Provenance::NullCount),
            })
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

        fn spread(&self, tests: &[Test]) -> Option<Spread> {
            self.asked.lock().expect("no test panics while holding this").extend_from_slice(tests);
            self.spread.map(|fraction| Spread { fraction, read: tests.len() })
        }

        fn extreme(&self, _column: usize, _end: End) -> Stat<Bound> {
            Stat::Unknown
        }

        fn nulls(&self, _column: usize) -> Stat<u64> {
            self.nulls
        }
    }

    /// A two column scan, whose columns the stub above numbers the other way round.
    fn bounded_scan() -> String {
        "Get memory.main.t AS t #0 [a::INTEGER, b::INTEGER]\n".to_string()
    }

    /// The estimate for a plan whose table zero is the given store.
    fn zoned(text: &str, rows: u64, zones: &Arc<Stub>) -> Stat<u64> {
        let stats = facts(&[("t", rows)]);
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
    fn a_range_the_store_can_interpolate_takes_its_fraction_rather_than_the_constant() {
        // The number that matters most in this file. Every range in TPC-H took a fifth whatever it
        // asked for, and `l_shipdate <= '1998-09-02'` keeps ninety eight percent of lineitem while
        // the two ends of the column sit in the footer saying 1992 and 1998.
        let text = format!("Filter (#0.0::INTEGER < 9::INTEGER)::BOOLEAN\n  {}", bounded_scan());
        let zones = Stub::spreading(0.98);
        assert_eq!(zoned(&text, 1_000_000, &zones), Stat::estimated(980_000, Provenance::ZoneMap));
    }

    #[test]
    fn a_fraction_above_the_constant_is_taken_as_readily_as_one_below_it() {
        // The rule is to use the number where there is one and not to make the answer smaller. All
        // sixteen ranges measured on TPC-H were underestimates, so the fifth was too small far more
        // often than it was too large, and a rule that only ever cut would have fixed none of them.
        let text = format!("Filter (#0.0::INTEGER < 9::INTEGER)::BOOLEAN\n  {}", bounded_scan());
        let narrow = Stub::spreading(0.01);
        assert_eq!(zoned(&text, 1_000_000, &narrow), Stat::estimated(10_000, Provenance::ZoneMap));
    }

    #[test]
    fn a_fraction_of_nothing_is_still_a_row_and_is_still_a_guess() {
        // The floor the guess has always had. A relation estimated away is a subtree nobody reads,
        // and an interpolated zero is an assumption about how values are spread rather than a fact
        // that the file holds none, so it does not get the exactness the ceiling of zero gets.
        let text = format!("Filter (#0.0::INTEGER < 9::INTEGER)::BOOLEAN\n  {}", bounded_scan());
        let empty = Stub::spreading(0.0);
        assert_eq!(zoned(&text, 1_000_000, &empty), Stat::estimated(1, Provenance::ZoneMap));
    }

    #[test]
    fn a_counted_condition_and_an_interpolated_one_report_the_arithmetic_over_both() {
        // Two suppliers answering two conditions of one filter. Neither name is the truth about
        // where the fraction came from, so the reader gets told it was worked out rather than read.
        let text = format!(
            "Filter ((#0.0::INTEGER = 5::INTEGER)::BOOLEAN AND (#0.1::INTEGER < 9::INTEGER)::BOOLEAN)::BOOLEAN\n  {}",
            bounded_scan()
        );
        let mut stats = facts(&[("t", 1_000_000)]);
        stats.record_distinct("memory", "main", "t", "a", 10, Provenance::Dictionary);
        let mut plan = Plan::parse(&text).expect("parses");
        let zones = Stub::spreading(0.5);
        plan.set_zones(0, Arc::clone(&zones) as Arc<dyn Zones>);
        let stat = rows_stat(&plan, plan.root(), &stats);
        // A tenth for the equality from the count, a half for the range from the bounds.
        assert_eq!(stat, Stat::estimated(50_000, Provenance::Propagation));
    }

    #[test]
    fn a_condition_the_store_could_not_read_still_costs_the_constant() {
        // Two conditions, one of which is a comparison of two columns and reads as no test at all.
        // The store answers for the one it was handed and says so, and the other is charged the
        // fifth on its own. Charging nothing for it would say a condition nobody can estimate keeps
        // every row, and charging the fifth twice would guess at a condition that was answered.
        let text = format!(
            "Filter ((#0.0::INTEGER < 9::INTEGER)::BOOLEAN AND (#0.0::INTEGER = #0.1::INTEGER)::BOOLEAN)::BOOLEAN\n  {}",
            bounded_scan()
        );
        // A half from the store and a fifth for the other one, which is a tenth of a million.
        let zones = Stub::spreading(0.5);
        assert_eq!(
            zoned(&text, 1_000_000, &zones),
            Stat::estimated(100_000, Provenance::Propagation)
        );
    }

    #[test]
    fn the_count_answers_an_equality_before_the_bounds_are_asked_to_interpolate_it() {
        // One value out of a range is not a fraction a range knows, so the store refuses equality
        // anyway, but the order is worth pinning: the count is the better number and goes first.
        let text = format!("Filter (#0.0::INTEGER = 5::INTEGER)::BOOLEAN\n  {}", bounded_scan());
        let mut stats = facts(&[("t", 1_000_000)]);
        stats.record_distinct("memory", "main", "t", "a", 8, Provenance::Dictionary);
        let mut plan = Plan::parse(&text).expect("parses");
        plan.set_zones(0, Stub::spreading(0.5) as Arc<dyn Zones>);
        assert_eq!(
            rows_stat(&plan, plan.root(), &stats),
            Stat::estimated(125_000, Provenance::Dictionary)
        );
    }

    #[test]
    fn the_column_the_store_is_asked_about_is_the_one_the_plan_named_and_not_the_position() {
        // The bug this mapping exists to stop. The plan's column zero is `a` and the store's
        // column zero is `b`, so a test handed straight through would rule out row groups on the
        // wrong column's bounds. That drops rows the query wanted, which is a wrong answer and not
        // a slow one.
        //
        // The store is asked twice for this filter, once for the ceiling and once for the
        // fraction, and both have to name the same column. Checking every ask rather than the
        // first means adding a third question later cannot quietly skip the mapping.
        let text = format!("Filter (#0.0::INTEGER < 9::INTEGER)::BOOLEAN\n  {}", bounded_scan());
        let zones = Stub::new(Some(100));
        zoned(&text, 1_000_000, &zones);
        let asked = zones.asked.lock().expect("not poisoned");
        assert!(!asked.is_empty(), "it was asked");
        for test in asked.iter() {
            assert_eq!(test.column, 1, "`a` is the store's column one");
            assert_eq!(test.op, Op::Less);
        }
    }

    #[test]
    fn a_table_with_no_store_recorded_is_estimated_the_way_it_always_was() {
        // Which is every table today except a `read_parquet` of one file, so this is the path
        // almost every query still takes and it has to be untouched.
        let text = format!("Filter (#0.0::INTEGER < 9::INTEGER)::BOOLEAN\n  {}", bounded_scan());
        assert_eq!(stat(&text, &[("t", 1_000_000)]), Stat::estimated(200_000, Provenance::Default));
    }

    #[test]
    fn a_null_test_on_a_column_the_store_counted_takes_the_count_over_the_constant() {
        // A hundred thousand rows with every tenth one null. The constant calls both of these a
        // fifth of the table, which is out by half in one direction and by four and a half times
        // in the other, and the store had the number the whole time.
        let null = format!(
            "Filter (#0.0::INTEGER IS NOT DISTINCT FROM NULL::INTEGER)::BOOLEAN\n  {}",
            bounded_scan()
        );
        let counted = Stub::counting(100_000, 10_000);
        assert_eq!(zoned(&null, 100_000, &counted), Stat::estimated(10_000, Provenance::NullCount));
        // The complement out of the same number, which is why both are here rather than only the
        // first. A filter that throws a tenth away used to be costed as throwing four fifths away.
        let present = format!(
            "Filter (#0.0::INTEGER IS DISTINCT FROM NULL::INTEGER)::BOOLEAN\n  {}",
            bounded_scan()
        );
        assert_eq!(
            zoned(&present, 100_000, &counted),
            Stat::estimated(90_000, Provenance::NullCount)
        );
    }

    #[test]
    fn a_store_that_states_no_null_count_gets_the_constant_it_always_got() {
        let text = format!(
            "Filter (#0.0::INTEGER IS NOT DISTINCT FROM NULL::INTEGER)::BOOLEAN\n  {}",
            bounded_scan()
        );
        let quiet = Stub::new(Some(100_000));
        assert_eq!(zoned(&text, 100_000, &quiet), Stat::estimated(20_000, Provenance::Default));
    }

    #[test]
    fn a_distinctness_test_between_two_columns_is_not_a_null_test() {
        // Same operator, and the null count says nothing at all about it. Reading this as a null
        // test would answer off a column the condition is not asking about.
        let text = format!(
            "Filter (#0.0::INTEGER IS DISTINCT FROM #0.1::INTEGER)::BOOLEAN\n  {}",
            bounded_scan()
        );
        let counted = Stub::counting(100_000, 10_000);
        assert_eq!(zoned(&text, 100_000, &counted), Stat::estimated(20_000, Provenance::Default));
    }

    /// A store that counted how many rows hold each value, from a list fixed when it is built.
    ///
    /// Numbered the other way round from the scan for the reason [`Stub`] is, and checked the same
    /// way: a test below reads back which column it was asked about.
    #[derive(Debug)]
    struct Counted {
        /// What [`Frequencies::rows`] answers.
        rows: u64,
        /// Per column of this store's own numbering, the counts it holds, and `None` for a column
        /// whose synopsis is missing or incomplete.
        held: Vec<Option<Vec<(i128, u64)>>>,
        /// What the synopsis of column `a` left out, and `None` for one that left nothing out.
        tail: Option<Remainder>,
        /// Every column it was asked about, so a test can check which one the estimator named.
        asked: Mutex<Vec<usize>>,
    }

    impl Counted {
        /// A store of a million and a half rows whose column `a` holds the given counts.
        fn of(held: Option<Vec<(i128, u64)>>) -> Arc<Self> {
            Arc::new(Self {
                rows: 1_500_000,
                held: vec![None, held],
                tail: None,
                asked: Mutex::new(Vec::new()),
            })
        }

        /// A store whose column `a` lists the given counts and left the given remainder out.
        fn prefixed(held: Vec<(i128, u64)>, tail: Remainder) -> Arc<Self> {
            Arc::new(Self {
                rows: 1_500_000,
                held: vec![None, Some(held)],
                tail: Some(tail),
                asked: Mutex::new(Vec::new()),
            })
        }
    }

    impl Frequencies for Counted {
        fn column(&self, name: &str) -> Option<usize> {
            match name {
                "b" => Some(0),
                "a" => Some(1),
                _ => None,
            }
        }

        fn rows(&self) -> u64 {
            self.rows
        }

        fn rows_with(&self, column: usize, value: &Bound) -> Stat<u64> {
            self.asked.lock().expect("no test panics while holding this").push(column);
            let (Some(Some(list)), Bound::Int(wanted)) = (self.held.get(column), value) else {
                return Stat::Unknown;
            };
            match list.iter().find(|(held, _)| held == wanted) {
                Some((_, of)) => Stat::exact(*of, Provenance::FrequencySynopsis),
                // A list that left something out says nothing about a value outside it, which is
                // what the remainder is for. A complete one says no rows hold it.
                None if self.tail.is_some() => Stat::Unknown,
                None => Stat::exact(0, Provenance::FrequencySynopsis),
            }
        }

        fn remainder(&self, column: usize) -> Option<Remainder> {
            self.tail.filter(|_| column == 1)
        }
    }

    /// The estimate for a plan whose table zero counted its values, and counted its distinct ones.
    ///
    /// Both, because the point of the synopsis is which of the two the estimate takes.
    fn common_stat(text: &str, distinct: u64, held: &Arc<Counted>) -> Stat<u64> {
        let stats = facts(&[("t", 1_500_000)]);
        let mut plan =
            Plan::parse(text).unwrap_or_else(|error| panic!("{text} did not parse: {error}"));
        plan.set_frequencies(0, Arc::clone(held) as Arc<dyn Frequencies>);
        plan.measure_distinct(0, "a", Stat::exact(distinct, Provenance::Dictionary));
        rows_stat(&plan, plan.root(), &stats)
    }

    #[test]
    fn an_equality_on_a_counted_column_takes_the_count_over_the_uniform_guess() {
        // TPC-H orders at scale factor 1. `o_orderstatus` holds three values over 1,500,000 rows
        // and 729,413 of them are `F`, which is not a third of anything. The uniformity assumption
        // says 500,000 for all three, and the writer counted the real number into the file.
        let text = format!("Filter (#0.0::INTEGER = 3::INTEGER)::BOOLEAN\n  {}", bounded_scan());
        let held = Counted::of(Some(vec![(3, 729_413), (4, 732_044), (5, 38_543)]));
        assert_eq!(
            common_stat(&text, 3, &held),
            Stat::estimated(729_413, Provenance::FrequencySynopsis)
        );
        // And the column it asked about is the store's, not the plan's. Asking about `b` here
        // would answer off the wrong column's counts, which is a wrong estimate arrived at
        // confidently.
        assert_eq!(*held.asked.lock().expect("not poisoned"), vec![1]);
    }

    #[test]
    fn a_value_a_complete_synopsis_does_not_list_is_as_close_to_no_rows_as_the_guess_goes() {
        // The half that is worth more than the counts. A synopsis that accounts for every row
        // proves no row holds a value it left out, so `= 9` is zero rows rather than a third of
        // the table. The estimate floors at one for the reason every guess here does: a relation
        // estimated away is a subtree nobody reads.
        let text = format!("Filter (#0.0::INTEGER = 9::INTEGER)::BOOLEAN\n  {}", bounded_scan());
        let held = Counted::of(Some(vec![(3, 729_413), (4, 732_044), (5, 38_543)]));
        assert_eq!(common_stat(&text, 3, &held).value(), Some(&1));
    }

    /// The counts of a column whose synopsis holds two values and dropped a thousand more.
    ///
    /// Three quarters of a million rows on one value and two hundred thousand on another, leaving
    /// 550,000 rows spread over the thousand values the writer could not keep.
    fn dropped(most: u64) -> Arc<Counted> {
        let held = vec![(3, 750_000), (4, 200_000)];
        Counted::prefixed(held, Remainder { rows: 550_000, listed: 2, most })
    }

    #[test]
    fn a_value_an_incomplete_synopsis_left_out_is_the_tail_spread_over_the_values_in_it() {
        // The shape a prefix is for, and the shape the uniformity assumption is worst at. One value
        // holds half the table, so dividing the rows by the distinct count calls every other value
        // 1,497 rows. The synopsis holds the two big ones exactly, which leaves 550,000 rows over
        // the thousand values it dropped, so a value out of that thousand is 550 rows.
        let text = format!("Filter {}\n  {}", equals(0, 9), bounded_scan());
        assert_eq!(
            common_stat(&text, 1002, &dropped(5_000)),
            Stat::estimated(550, Provenance::Propagation)
        );
    }

    #[test]
    fn the_bound_the_writer_recorded_caps_what_the_tail_is_spread_into() {
        // The pass proved no value it dropped holds more than a hundred rows, so a tail that divides
        // out to 550 is a hundred. The bound is a fact about the rows and the division is a guess
        // about them, and where the two disagree the fact wins.
        let text = format!("Filter {}\n  {}", equals(0, 9), bounded_scan());
        assert_eq!(common_stat(&text, 1002, &dropped(100)).value(), Some(&100));
    }

    #[test]
    fn a_tail_too_small_to_divide_is_still_one_row_rather_than_none() {
        // Five rows over a thousand values rounds to nothing, and nothing is the one answer that
        // cannot be right: the value is in the column's distinct count, so some row holds it.
        let text = format!("Filter {}\n  {}", equals(0, 9), bounded_scan());
        let held = Counted::prefixed(
            vec![(3, 750_000), (4, 749_995)],
            Remainder { rows: 5, listed: 2, most: 10 },
        );
        assert_eq!(common_stat(&text, 1002, &held).value(), Some(&1));
    }

    #[test]
    fn a_value_an_incomplete_synopsis_does_list_is_still_the_count_it_listed() {
        // The other half, unchanged by any of this. A prefix's counts are exact because the writer
        // recounts what survived its pass, so a listed value needs no remainder and the provenance
        // says one store rather than two.
        let text = format!("Filter {}\n  {}", equals(0, 3), bounded_scan());
        assert_eq!(
            common_stat(&text, 1002, &dropped(5_000)),
            Stat::estimated(750_000, Provenance::FrequencySynopsis)
        );
    }

    #[test]
    fn an_in_list_over_a_prefix_adds_the_counts_it_has_to_the_tail_it_guesses() {
        // `a IN (3, 9)` where the synopsis lists 3 and dropped 9. Each branch is answered by
        // whichever half of the synopsis can answer it and the two are summed as they always were.
        // 750,550 of 1,500,000 rows, a row short of it once the sum has been through a fraction and
        // back, because a filter's answer is carried as the share of its input that it keeps.
        let text = one_of(0, &[3, 9]);
        assert_eq!(
            common_stat(&text, 1002, &dropped(5_000)),
            Stat::estimated(750_549, Provenance::Propagation)
        );
    }

    #[test]
    fn a_distinct_count_below_what_the_synopsis_listed_falls_back_to_the_bound() {
        // Two reads of one column contradicting each other. The synopsis lists two values and the
        // catalog says the column holds two, which leaves nowhere for the 550,000 rows the synopsis
        // did not account for. Spreading them over no values is a division by zero, so the answer is
        // the bound the pass proved, which is a fact about the rows rather than arithmetic on a
        // disagreement between two readers.
        let text = format!("Filter {}\n  {}", equals(0, 9), bounded_scan());
        assert_eq!(
            common_stat(&text, 2, &dropped(5_000)),
            Stat::estimated(5_000, Provenance::Propagation)
        );
    }

    /// The same as [`common_stat`] where what anybody knows about the values is a ceiling.
    fn ceilinged(text: &str, distinct: u64, held: &Arc<Counted>) -> Stat<u64> {
        let stats = facts(&[("t", 1_500_000)]);
        let mut plan =
            Plan::parse(text).unwrap_or_else(|error| panic!("{text} did not parse: {error}"));
        plan.set_frequencies(0, Arc::clone(held) as Arc<dyn Frequencies>);
        plan.measure_distinct(
            0,
            "a",
            Stat::certified(distinct, 1.0, Direction::AtMost, Provenance::ZoneMap),
        );
        rows_stat(&plan, plan.root(), &stats)
    }

    #[test]
    fn a_ceiling_on_the_values_is_not_a_count_and_does_not_divide_the_tail() {
        // What a native file's integer column gets: the span between its two ends, which is a
        // ceiling and not a count. Dividing by it divides by more values than the column has and
        // lands under the truth by exactly that factor, so the bound answers instead. A thousand
        // values inside a span of ten thousand is a tenth, and a tail estimated at a tenth of its
        // size is a filter the planner thinks is ten times more selective than it is.
        let text = format!("Filter {}\n  {}", equals(0, 9), bounded_scan());
        assert_eq!(
            ceilinged(&text, 10_002, &dropped(5_000)),
            Stat::estimated(5_000, Provenance::Propagation)
        );
        // And an exact count of the same column does divide, which is the pair this rests on.
        assert_eq!(common_stat(&text, 1002, &dropped(5_000)).value(), Some(&550));
    }

    /// The same as [`common_stat`] for a table that also kept bounds, and so also a null count.
    ///
    /// Both stores, because `<>` is the one shape here that needs a number out of each of them.
    fn common_stat_of(text: &str, held: &Arc<Counted>, nulls: u64) -> Stat<u64> {
        let stats = facts(&[("t", 1_500_000)]);
        let mut plan =
            Plan::parse(text).unwrap_or_else(|error| panic!("{text} did not parse: {error}"));
        plan.set_frequencies(0, Arc::clone(held) as Arc<dyn Frequencies>);
        plan.set_zones(0, Stub::counting(1_500_000, nulls) as Arc<dyn Zones>);
        plan.measure_distinct(0, "a", Stat::exact(3, Provenance::Dictionary));
        rows_stat(&plan, plan.root(), &stats)
    }

    /// One equality on the plan's column `column` against `value`, as the plan printer writes it.
    fn equals(column: u32, value: i64) -> String {
        format!("(#0.{column}::INTEGER = {value}::INTEGER)::BOOLEAN")
    }

    /// A filter over an `OR` of the given branches, which is the shape an `IN` list binds to.
    fn any_of(branches: &[String]) -> String {
        format!("Filter ({})::BOOLEAN\n  {}", branches.join(" OR "), bounded_scan())
    }

    /// A filter over an `IN` list of `values` on the plan's column `column`.
    fn one_of(column: u32, values: &[i64]) -> String {
        let branches: Vec<String> = values.iter().map(|value| equals(column, *value)).collect();
        any_of(&branches)
    }

    #[test]
    fn an_inequality_on_a_counted_column_is_the_rows_less_that_value_and_less_the_nulls() {
        // The complement, and not of the count alone. `o_orderstatus <> 'F'` is null for a row whose
        // status is null, and a null does not pass a filter, so the rows that pass are the ones the
        // synopsis did not count under `F` minus the ones that hold nothing at all.
        let text = format!("Filter (#0.0::INTEGER <> 3::INTEGER)::BOOLEAN\n  {}", bounded_scan());
        let held = Counted::of(Some(vec![(3, 729_413), (4, 732_044), (5, 26_543)]));
        assert_eq!(
            common_stat_of(&text, &held, 12_000),
            Stat::estimated(758_587, Provenance::FrequencySynopsis)
        );
    }

    #[test]
    fn an_inequality_gives_up_where_the_store_states_no_null_count() {
        // Without the null count there is no honest complement, since the rows that pass are the
        // ones that are neither the value nor null and one of those two numbers is missing. So this
        // keeps the flat fifth, which is what a `<>` got before any of this: the distinct count says
        // how many rows hold one value and nothing about how many hold anything else.
        let text = format!("Filter (#0.0::INTEGER <> 3::INTEGER)::BOOLEAN\n  {}", bounded_scan());
        let held = Counted::of(Some(vec![(3, 729_413), (4, 732_044), (5, 38_543)]));
        assert_eq!(common_stat(&text, 3, &held), Stat::estimated(300_000, Provenance::Default));
    }

    #[test]
    fn a_list_of_values_on_a_counted_column_is_the_counts_added_up() {
        // `o_orderstatus IN ('F', 'P')` binds to an `OR` of two equalities, and two counts out of
        // one synopsis add. The uniform guess has no arithmetic for a disjunction at all and gives
        // the whole thing the flat fifth, which here is under half the truth.
        let text = one_of(0, &[3, 5]);
        let held = Counted::of(Some(vec![(3, 729_413), (4, 732_044), (5, 38_543)]));
        assert_eq!(
            common_stat_of(&text, &held, 0),
            Stat::estimated(767_956, Provenance::FrequencySynopsis)
        );
    }

    #[test]
    fn a_disjunction_across_two_columns_is_not_a_list_and_is_not_added_up() {
        // `a = 3 OR b = 4` is not an `IN` list. The rows that pass are somewhere between the larger
        // count and the sum of the two, and which depends on how the columns go together, which no
        // per column synopsis says. So this keeps the flat fifth rather than guessing confidently.
        let text = any_of(&[equals(0, 3), equals(1, 4)]);
        let held = Counted::of(Some(vec![(3, 729_413), (4, 732_044), (5, 38_543)]));
        assert_eq!(common_stat_of(&text, &held, 0), Stat::estimated(300_000, Provenance::Default));
    }

    #[test]
    fn a_list_longer_than_the_cap_is_left_to_the_constant() {
        // A generated list can be thousands long, and each value is a walk of the synopsis. Past a
        // handful the sum has stopped saying anything the column's own count would not, so the cap
        // is where the reading stops rather than something to pay for.
        let values: Vec<i64> = (0..33).collect();
        let held = Counted::of(Some(vec![(3, 729_413), (4, 732_044), (5, 38_543)]));
        assert_eq!(
            common_stat_of(&one_of(0, &values), &held, 0),
            Stat::estimated(300_000, Provenance::Default)
        );
    }

    #[test]
    fn a_column_with_no_synopsis_is_divided_by_its_distinct_count_the_way_it_always_was() {
        let text = format!("Filter (#0.0::INTEGER = 3::INTEGER)::BOOLEAN\n  {}", bounded_scan());
        assert_eq!(
            common_stat(&text, 3, &Counted::of(None)),
            Stat::estimated(500_000, Provenance::Dictionary)
        );
    }
}
