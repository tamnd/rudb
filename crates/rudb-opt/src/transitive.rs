//! Transitive predicates.
//!
//! `a.x = b.x AND a.x > 5` says something about `b.x` that nobody wrote down. Writing it down is
//! what lets the predicate reach the second table, and reaching a table is the only thing filter
//! pushdown can do for a query. Without this, a query that joins on a key and restricts one side of
//! it reads all of the other side, which on ClickBench is the difference between touching a column
//! and touching a file.
//!
//! The rule is equality between two columns. Every predicate that reads one of those columns and
//! nothing else is copied with the other column in its place. That is it. `a.x + 1 = b.x` derives
//! nothing, because the equality is between an expression and a column and substituting one for the
//! other would need the inverse of the expression, and the pinned binary derives nothing there
//! either. A predicate that reads the column and another column beside it derives nothing, because
//! the copy would read a column the side it is going to does not have.
//!
//! # Where the equalities come from
//!
//! Two places, and they are not the same question.
//!
//! A set of predicates that all have to hold at one point, which is what a filter is, implies
//! anything they imply together. So [`within`] adds what it derives to the set and the pushdown that
//! follows decides where each one goes, with every rule it already has still deciding. That covers
//! `WHERE a.x = b.x AND a.x > 5` over a cross product, and it covers `WHERE x = y AND x > 5` over one
//! table, which the binary also does.
//!
//! A join condition is not a predicate that holds at one point. It holds of the pairs the join made,
//! and for an outer join it says nothing at all about the rows that did not pair. So [`across`] is a
//! different rule with a different table behind it. What it derives can only be used to drop a row
//! that could never have paired with anything, and [`droppable`] is where each join kind says whether
//! dropping such a row is allowed. It is allowed for the side an outer join invents nulls for, since
//! a row of that side that cannot match is a row that contributes nothing either way, and it is not
//! allowed for the side the join is named after, where an unmatched row still comes out padded. That
//! is what sends `WHERE a.x > 5` over a left join into `b` while nothing ever sends anything into `a`.
//!
//! # Why it does not run twice
//!
//! A derived predicate is a predicate the set already implies, so deriving a second time from a set
//! that has it would write it down twice, and a pass whose output depends on how many times it has
//! run is a pass [`crate::optimize_with`] fails an assertion on. Two things stop that. [`walk::same`]
//! compares a new predicate against the ones already in hand, which is what makes [`within`] stable,
//! since everything it derives stays in the same set it derived from. And [`across`] only derives
//! from predicates that are moving, never from the join's own conditions, because a condition stays
//! where it is and what is derived from it goes somewhere the next run cannot see.
//!
//! # What it does not reach
//!
//! One join at a time. `a` joined to `b` on a key and `b` joined to `c` on the same key, with a
//! predicate on `a`, reaches `b` and stops, because the equality that would carry it to `c` is in the
//! other join and this only ever looks at one. Reaching `c` needs the equalities of a whole run of
//! inner joins collected into equivalence classes first, which is the same structure join ordering
//! needs and is why it waits for it. A predicate written into an `ON` clause rather than a `WHERE`
//! is the other one, for the reason in the section above. Both are #215.

use rudb_plan::{ColumnBinding, CompareOp, Expr, ExprRef, JoinKind, Plan};

use crate::filter::kept;
use crate::tables::{TableSet, Tables};
use crate::walk;

/// Adds to `parts` every predicate the equalities inside `parts` imply about the rest of it.
///
/// Sound wherever the whole of `parts` has to hold, which is every filter. Adding a conjunct that
/// the other conjuncts already imply cannot change which rows pass, so nothing here has to know what
/// is under the filter or what is above it.
pub(crate) fn within(plan: &mut Plan, parts: &mut Vec<ExprRef>) {
    let pairs = equalities(plan, parts);
    if pairs.is_empty() {
        return;
    }
    let sources = parts.clone();
    for (one, other) in pairs {
        for &source in &sources {
            for (from, to) in [(one, other), (other, one)] {
                let Some(made) = copy(plan, source, from, to) else { continue };
                if !parts.iter().any(|&held| walk::same(plan, held, made)) {
                    parts.push(made);
                }
            }
        }
    }
}

/// What the conditions of a join imply about each side, as extra predicates for that side.
///
/// The two lists are for the left and the right, and each one is already written against the columns
/// that side produces, so the caller hands them straight to the recursion rather than sorting them
/// again.
///
/// The equalities come from the join's conditions and the predicates come from `pending`, which is
/// what sits above the join and is on its way down. A predicate above the join only counts as
/// something that holds of a pair when the side it reads is a side the join hands on as it is, which
/// is [`kept`], the same table pushdown itself uses.
pub(crate) fn across(
    plan: &mut Plan,
    tables: &mut Tables,
    kind: JoinKind,
    conditions: &[ExprRef],
    pending: &[ExprRef],
    below: (&TableSet, &TableSet),
) -> (Vec<ExprRef>, Vec<ExprRef>) {
    let drop = droppable(kind);
    let empty = (Vec::new(), Vec::new());
    if !drop.0 && !drop.1 {
        return empty;
    }
    let pairs = equalities(plan, conditions);
    if pairs.is_empty() {
        return empty;
    }

    let keep = kept(kind);
    let mut sources = Vec::new();
    for &part in pending {
        let read = tables.of(plan, part);
        let held = (keep.0 && read.is_subset_of(below.0)) || (keep.1 && read.is_subset_of(below.1));
        if held {
            sources.push(part);
        }
    }

    let (mut to_left, mut to_right) = empty;
    for (one, other) in pairs {
        for &source in &sources {
            for (from, to) in [(one, other), (other, one)] {
                let Expr::Column(binding) = *plan.expr(to) else { continue };
                let (allowed, into) = if below.0.contains(binding.table) {
                    (drop.0, &mut to_left)
                } else if below.1.contains(binding.table) {
                    (drop.1, &mut to_right)
                } else {
                    continue;
                };
                if !allowed {
                    continue;
                }
                let Some(made) = copy(plan, source, from, to) else { continue };
                let seen = |plan: &Plan, held: &[ExprRef]| {
                    held.iter().any(|&held| walk::same(plan, held, made))
                };
                if !seen(plan, pending) && !seen(plan, conditions) && !seen(plan, into) {
                    into.push(made);
                }
            }
        }
    }
    (to_left, to_right)
}

/// Which sides of a join may lose a row that could never have paired with anything.
///
/// The other half of [`kept`], and not its opposite. `kept` asks whether a row that fails a
/// predicate can go, which is true of the side an outer join is named after and false of the side it
/// invents nulls for. This asks whether a row that has no partner can go, which is the other way
/// around: an unmatched row of the named side still comes out padded and has to stay, and an
/// unmatched row of the other side was never going to appear anywhere.
///
/// An inner join answers yes twice, since a row that fails either question is a row it drops itself.
/// A full outer join answers no twice, because both of its sides come out padded. A positional join
/// answers no twice because it pairs by position, so a row that could never have paired is not a
/// thing it has.
fn droppable(kind: JoinKind) -> (bool, bool) {
    match kind {
        JoinKind::Inner => (true, true),
        JoinKind::Left | JoinKind::Semi | JoinKind::Anti | JoinKind::Single => (false, true),
        JoinKind::Right => (true, false),
        JoinKind::Full | JoinKind::Positional => (false, false),
    }
}

/// The pairs of columns the equalities in `parts` say are equal.
///
/// Both sides have to be a column and the two have to have the same type. A comparison across two
/// types is a comparison the binder already decided how to do, and putting one of them where the
/// other was would hand the kernels a pair it was never asked about.
fn equalities(plan: &Plan, parts: &[ExprRef]) -> Vec<(ExprRef, ExprRef)> {
    let mut pairs = Vec::new();
    for &part in parts {
        let Expr::Compare { op: CompareOp::Equal, left, right } = *plan.expr(part) else {
            continue;
        };
        let (Expr::Column(one), Expr::Column(other)) = (plan.expr(left), plan.expr(right)) else {
            continue;
        };
        if one != other && plan.expr_type(left) == plan.expr_type(right) {
            pairs.push((left, right));
        }
    }
    pairs
}

/// `source` with the column `from` reads replaced by `to`, when that is a thing worth writing down.
///
/// Three ways to get nothing back. The source reads a column other than `from`, so the copy would
/// read a column from two sides at once. The source reads no column at all, so the copy is the
/// source. The source is volatile, so the copy is a second call rather than a second look.
///
/// The expression is built before the caller can find out it already had one. That leaves it in the
/// arena with nothing pointing at it, which is what this whole pass does to every filter it takes
/// apart, and the alternative is a second walk that compares without building.
fn copy(plan: &mut Plan, source: ExprRef, from: ExprRef, to: ExprRef) -> Option<ExprRef> {
    let Expr::Column(from) = *plan.expr(from) else { return None };
    if !only(plan, source, from) || walk::volatile(plan, source) {
        return None;
    }
    Some(replace(plan, source, from, to))
}

/// Whether `expr` reads `binding` and reads nothing else.
fn only(plan: &Plan, expr: ExprRef, binding: ColumnBinding) -> bool {
    let mut found = false;
    let mut other = false;
    walk::columns(plan, expr, &mut |read| {
        if read == binding {
            found = true;
        } else {
            other = true;
        }
    });
    found && !other
}

/// Rewrites every reference to `from` in `expr` into `to`.
fn replace(plan: &mut Plan, expr: ExprRef, from: ColumnBinding, to: ExprRef) -> ExprRef {
    if matches!(*plan.expr(expr), Expr::Column(binding) if binding == from) {
        return to;
    }
    walk::rebuild(plan, expr, &mut |plan, child| replace(plan, child, from, to))
}

#[cfg(test)]
mod tests {
    use crate::filter::FilterPushdown;
    use crate::pass::{Context, Pass};
    use rudb_plan::Plan;

    /// The plan a text prints as after pushdown, having run it twice.
    ///
    /// Twice because deriving a predicate the query already implies is how this rule would produce a
    /// different plan each time it ran, and a pass that does that turns `optimize_with`'s idempotence
    /// assertion into a coin toss rather than a check.
    fn pushed(text: &str) -> String {
        let mut plan =
            Plan::parse(text).unwrap_or_else(|error| panic!("{text} did not parse: {error}"));
        let mut once = String::new();
        for _ in 0..2 {
            FilterPushdown
                .run(&mut plan, &Context::new())
                .unwrap_or_else(|error| panic!("{text} did not push: {error}"));
            plan.validate().unwrap_or_else(|error| panic!("{text} pushed to a bad plan: {error}"));
            if once.is_empty() {
                once = plan.to_string();
            }
        }
        assert_eq!(once, plan.to_string(), "{text} did not print the same the second time");
        once
    }

    #[test]
    fn an_equality_between_two_columns_of_one_table_copies_a_predicate_across_it() {
        let before = "\
Filter ((#0.0::INTEGER = #0.1::INTEGER)::BOOLEAN AND (#0.0::INTEGER > 5::INTEGER)::BOOLEAN)::BOOLEAN
  Get memory.main.t AS t #0 [a::INTEGER, b::INTEGER]
";
        let after = "\
Filter ((#0.0::INTEGER = #0.1::INTEGER)::BOOLEAN AND (#0.0::INTEGER > 5::INTEGER)::BOOLEAN AND (#0.1::INTEGER > 5::INTEGER)::BOOLEAN)::BOOLEAN
  Get memory.main.t AS t #0 [a::INTEGER, b::INTEGER]
";
        assert_eq!(pushed(before), after);
    }

    #[test]
    fn an_equality_over_a_cross_product_sends_the_predicate_into_both_sides() {
        // The one that matters. Neither half of this reaches `b` without the other: the equality is
        // over both sides so it cannot move, and the comparison is over `a` so it has nothing to say
        // about `b` until the equality says it.
        let before = "\
Filter ((#0.0::INTEGER = #1.0::INTEGER)::BOOLEAN AND (#0.0::INTEGER > 5::INTEGER)::BOOLEAN)::BOOLEAN
  CrossProduct
    Get memory.main.t AS a #0 [a::INTEGER]
    Get memory.main.t AS b #1 [a::INTEGER]
";
        let after = "\
Filter (#0.0::INTEGER = #1.0::INTEGER)::BOOLEAN
  CrossProduct
    Filter (#0.0::INTEGER > 5::INTEGER)::BOOLEAN
      Get memory.main.t AS a #0 [a::INTEGER]
    Filter (#1.0::INTEGER > 5::INTEGER)::BOOLEAN
      Get memory.main.t AS b #1 [a::INTEGER]
";
        assert_eq!(pushed(before), after);
    }

    #[test]
    fn a_join_condition_sends_a_predicate_over_one_side_into_the_other() {
        let before = "\
Filter (#0.0::INTEGER > 5::INTEGER)::BOOLEAN
  Join INNER on=[(#0.0::INTEGER = #1.0::INTEGER)::BOOLEAN]
    Get memory.main.t AS a #0 [a::INTEGER]
    Get memory.main.t AS b #1 [a::INTEGER]
";
        let after = "\
Join INNER on=[(#0.0::INTEGER = #1.0::INTEGER)::BOOLEAN]
  Filter (#0.0::INTEGER > 5::INTEGER)::BOOLEAN
    Get memory.main.t AS a #0 [a::INTEGER]
  Filter (#1.0::INTEGER > 5::INTEGER)::BOOLEAN
    Get memory.main.t AS b #1 [a::INTEGER]
";
        assert_eq!(pushed(before), after);
    }

    #[test]
    fn a_left_join_takes_the_derived_predicate_on_the_side_it_pads() {
        // `WHERE a.x > 5` cannot be pushed into `b`, because a row of `b` that fails it may still be
        // a row some surviving `a` row pairs with. `b.x > 5` can, because a `b` row that fails it
        // pairs with no `a` row that survives, and an `a` row left unpaired comes out padded either
        // way.
        let before = "\
Filter (#0.0::INTEGER > 5::INTEGER)::BOOLEAN
  Join LEFT on=[(#0.0::INTEGER = #1.0::INTEGER)::BOOLEAN]
    Get memory.main.t AS a #0 [a::INTEGER]
    Get memory.main.t AS b #1 [a::INTEGER]
";
        let after = "\
Join LEFT on=[(#0.0::INTEGER = #1.0::INTEGER)::BOOLEAN]
  Filter (#0.0::INTEGER > 5::INTEGER)::BOOLEAN
    Get memory.main.t AS a #0 [a::INTEGER]
  Filter (#1.0::INTEGER > 5::INTEGER)::BOOLEAN
    Get memory.main.t AS b #1 [a::INTEGER]
";
        assert_eq!(pushed(before), after);
    }

    #[test]
    fn a_condition_over_the_padded_side_of_a_left_join_derives_nothing_for_the_other() {
        // `ON b.x > 5` is not something the rows of `a` have to satisfy. An `a` row that fails it
        // comes out padded rather than not at all, so copying it onto `a` would delete rows.
        let before = "\
Join LEFT on=[(#0.0::INTEGER = #1.0::INTEGER)::BOOLEAN, (#1.0::INTEGER > 5::INTEGER)::BOOLEAN]
  Get memory.main.t AS a #0 [a::INTEGER]
  Get memory.main.t AS b #1 [a::INTEGER]
";
        assert_eq!(pushed(before), before);
    }

    #[test]
    fn a_predicate_written_into_the_on_clause_is_not_a_source() {
        // The binary derives `b.x > 5` here and this does not, which is #215. A condition stays in
        // the `ON` clause however many times the pass runs, and what is derived from it is pushed
        // into a side where the next run cannot see it, so deriving from one writes a second copy
        // every time the sequence goes round.
        let before = "\
Join LEFT on=[(#0.0::INTEGER = #1.0::INTEGER)::BOOLEAN, (#0.0::INTEGER > 5::INTEGER)::BOOLEAN]
  Get memory.main.t AS a #0 [a::INTEGER]
  Get memory.main.t AS b #1 [a::INTEGER]
";
        assert_eq!(pushed(before), before);
    }

    #[test]
    fn a_full_join_derives_nothing_because_both_of_its_sides_come_out_padded() {
        // `a.a IS NULL` rather than a comparison, since a comparison over either side of a full join
        // is a predicate that makes it a one sided join through `crate::nulls`, and the point here
        // is the join that stays full.
        let before = "\
Filter (#0.0::INTEGER IS NOT DISTINCT FROM NULL::\"NULL\")::BOOLEAN
  Join FULL on=[(#0.0::INTEGER = #1.0::INTEGER)::BOOLEAN]
    Get memory.main.t AS a #0 [a::INTEGER]
    Get memory.main.t AS b #1 [a::INTEGER]
";
        assert_eq!(pushed(before), before);
    }

    #[test]
    fn an_equality_between_an_expression_and_a_column_derives_nothing() {
        // Undoing this one needs the inverse of the expression, which is a different rule that the
        // pinned binary does not have either.
        let before = "\
Filter (#0.0::INTEGER > 5::INTEGER)::BOOLEAN
  Join INNER on=[(\"+\"(#0.0::INTEGER, 1::INTEGER)::INTEGER = #1.0::INTEGER)::BOOLEAN]
    Get memory.main.t AS a #0 [a::INTEGER]
    Get memory.main.t AS b #1 [a::INTEGER]
";
        let after = "\
Join INNER on=[(\"+\"(#0.0::INTEGER, 1::INTEGER)::INTEGER = #1.0::INTEGER)::BOOLEAN]
  Filter (#0.0::INTEGER > 5::INTEGER)::BOOLEAN
    Get memory.main.t AS a #0 [a::INTEGER]
  Get memory.main.t AS b #1 [a::INTEGER]
";
        assert_eq!(pushed(before), after);
    }

    #[test]
    fn a_predicate_reading_a_second_column_derives_nothing() {
        // `a.x > a.y` copied onto `b` would read `a.y` from a side that does not have it.
        let before = "\
Filter (#0.0::INTEGER > #0.1::INTEGER)::BOOLEAN
  Join INNER on=[(#0.0::INTEGER = #1.0::INTEGER)::BOOLEAN]
    Get memory.main.t AS a #0 [a::INTEGER, b::INTEGER]
    Get memory.main.t AS b #1 [a::INTEGER]
";
        let after = "\
Join INNER on=[(#0.0::INTEGER = #1.0::INTEGER)::BOOLEAN]
  Filter (#0.0::INTEGER > #0.1::INTEGER)::BOOLEAN
    Get memory.main.t AS a #0 [a::INTEGER, b::INTEGER]
  Get memory.main.t AS b #1 [a::INTEGER]
";
        assert_eq!(pushed(before), after);
    }

    #[test]
    fn a_derived_predicate_reaches_the_next_table_and_stops_there() {
        // The binary puts `c.x > 5` on the third table too. Carrying it that far means holding the
        // equalities of every join in a run of them at once rather than one join's worth, which is
        // the equivalence class structure join ordering needs anyway, so it is #215 and not this.
        let before = "\
Filter (#0.0::INTEGER > 5::INTEGER)::BOOLEAN
  Join INNER on=[(#1.0::INTEGER = #2.0::INTEGER)::BOOLEAN]
    Join INNER on=[(#0.0::INTEGER = #1.0::INTEGER)::BOOLEAN]
      Get memory.main.t AS a #0 [a::INTEGER]
      Get memory.main.t AS b #1 [a::INTEGER]
    Get memory.main.t AS c #2 [a::INTEGER]
";
        let after = "\
Join INNER on=[(#1.0::INTEGER = #2.0::INTEGER)::BOOLEAN]
  Join INNER on=[(#0.0::INTEGER = #1.0::INTEGER)::BOOLEAN]
    Filter (#0.0::INTEGER > 5::INTEGER)::BOOLEAN
      Get memory.main.t AS a #0 [a::INTEGER]
    Filter (#1.0::INTEGER > 5::INTEGER)::BOOLEAN
      Get memory.main.t AS b #1 [a::INTEGER]
  Get memory.main.t AS c #2 [a::INTEGER]
";
        assert_eq!(pushed(before), after);
    }
}
