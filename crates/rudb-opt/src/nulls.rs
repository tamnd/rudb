//! What a predicate says about a row that a join padded with nulls.
//!
//! An outer join produces two kinds of row. A matched row has values from both sides, and it is the
//! row an inner join would have produced. A padded row is a row from the kept side with nulls where
//! the other side's columns go, and it is the only reason the join is not an inner join. So a filter
//! above the join that cannot be true of a padded row is a filter that throws every padded row away,
//! and a join whose padded rows are all thrown away was an inner join written the long way.
//!
//! That matters more than the word in the query suggests. An inner join may be reordered with the
//! joins around it and an outer join may not, a predicate may be pushed into either side of an inner
//! join and only into the kept side of an outer one, and a `WHERE` clause that names a column of the
//! other side is a thing people write without meaning anything by it. `spec/09-optimizer.md` section
//! 9.3 asks for the rewrite and this is the analysis under it.
//!
//! # What a padded row looks like
//!
//! Every column of the padded side is null at once. Not one of them, all of them, which is what
//! makes the question answerable: the analysis is handed the set of tables that went null and asks
//! what the predicate evaluates to when every column binding into that set is null. The columns from
//! the other side are ordinary values it knows nothing about.
//!
//! The answer is one of six things. Null, true or false when the row decides it on its own, dropped
//! when it is null or false without which of the two being clear, present when the value is known not
//! to be null and nothing more than that, and unknown when the predicate reads a column the padding
//! did not touch. A predicate rejects the padded row on any of the first three but the true one,
//! since `WHERE` keeps the rows where the predicate is true and a null predicate drops the row the
//! same way a false one does.
//!
//! Unknown is the answer for anything not proven, which is the whole safety argument. A rule that
//! guesses here turns a left join into an inner one and loses rows, so every arm that is not certain
//! says unknown and the join stays as it was written.
//!
//! # Where the six values come from
//!
//! A comparison is null when either side is null, which is what makes `WHERE b.x > 5` reject: `b.x`
//! is null, so the comparison is null, so the row goes. `IS DISTINCT FROM` and `IS NOT DISTINCT
//! FROM` are the two that are not, since they are total and answer true or false about nulls rather
//! than answering null. Those two are how `IS NULL` and `IS NOT NULL` reach here, because the binder
//! writes both as a comparison against a null constant, and they are the reason the analysis tracks
//! present as well as null: `b.x IS NOT NULL` is a comparison of one known null against one known
//! non-null, which is false, which rejects.
//!
//! `AND` rejects when any part of it does and `OR` when every part of it does, which is where the
//! dropped answer earns its place: one part of an `AND` being null and the rest saying nothing is an
//! `AND` that is null or false and never true. A function is null when an argument is null, except
//! for the ones in [`TOLERANT`], and that default is the direction DuckDB's own functions take: a
//! function that wants to see a null argument says so, and one that has not said so never sees one.
//!
//! # What it does not do
//!
//! It reads the filters above the join and not the join's own conditions. A condition decides which
//! rows pair up and is applied while they are pairing, so a padded row has already satisfied
//! everything the `ON` clause had to say by the time it exists, and asking what the condition says
//! about it is asking the wrong question.
//!
//! A `CASE` is always unknown, and so is an aggregate. `CASE WHEN b.x IS NULL THEN 1 ELSE 0 END = 1`
//! is a predicate this could answer and does not, because a `CASE` in a `WHERE` clause over an outer
//! join is not a shape worth the arms until something measures it.

use rudb_common::Value;
use rudb_plan::{CompareOp, ConjunctionOp, Expr, ExprRef, JoinKind, Plan, Slice};

use crate::tables::TableSet;

/// The functions that can answer something other than null when an argument of theirs is null.
///
/// Everything else is taken to answer null, which is what DuckDB's scalar functions do unless they
/// ask not to. The list is short because rudb has few functions, and the way it goes wrong is a
/// function added here that quietly swallows a null, so a function that has an opinion about null
/// arguments belongs in this list on the same commit that adds it.
pub const TOLERANT: [&str; 1] = ["coalesce"];

/// What a predicate evaluates to over a row whose columns from one side are all null.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Known {
    /// Null.
    Null,
    /// True.
    True,
    /// False.
    False,
    /// Null or false, without knowing which.
    ///
    /// `WHERE` keeps the rows where the predicate is true, so those two are one answer as far as
    /// the row is concerned, and a great many predicates are provably one of them without being
    /// provably either. `b.x > 5 AND a.y > 5` over a row whose `b.x` is null is null when `a.y > 5`
    /// and false when it is not, and the row goes either way.
    Dropped,
    /// Not null, and nothing more than that.
    Present,
    /// Anything at all, which is the answer for whatever is not proven.
    Unknown,
}

impl Known {
    /// Whether a `WHERE` clause throws the row away.
    fn dropped(self) -> bool {
        matches!(self, Self::Null | Self::False | Self::Dropped)
    }

    /// Whether the value is known not to be null.
    fn present(self) -> bool {
        matches!(self, Self::True | Self::False | Self::Present)
    }
}

/// The join kind this join really is, given the predicates sitting above it.
///
/// A left join keeps every left row and pads the right, so a predicate that rejects a right-padded
/// row leaves only the rows that matched, which is an inner join. A right join is the same sentence
/// with the sides swapped. A full join produces both kinds of padded row and loses one kind at a
/// time: rejecting on the left takes away the rows that came from an unmatched right row, and what
/// is left is every left row with its match or with nulls, which is a left join.
///
/// Every other kind is handed back as it was. A semi join and an anti join pad nothing, a single
/// join carries the at most one row rule that an inner join does not, and a positional join pairs by
/// position rather than by a condition, so none of them is the join this would turn it into.
pub(crate) fn narrow(
    plan: &Plan,
    kind: JoinKind,
    pending: &[ExprRef],
    below: (&TableSet, &TableSet),
) -> JoinKind {
    let padded = match kind {
        JoinKind::Left => (false, true),
        JoinKind::Right => (true, false),
        JoinKind::Full => (true, true),
        _ => return kind,
    };
    let gone = |side: &TableSet| pending.iter().any(|&part| rejects(plan, part, side));
    let (left, right) = (padded.0 && gone(below.0), padded.1 && gone(below.1));
    match (kind, left, right) {
        (JoinKind::Left, _, true) | (JoinKind::Right, true, _) | (JoinKind::Full, true, true) => {
            JoinKind::Inner
        }
        (JoinKind::Full, true, false) => JoinKind::Left,
        (JoinKind::Full, false, true) => JoinKind::Right,
        _ => kind,
    }
}

/// Whether `expr` is null or false for every row whose columns from `nulled` are all null.
fn rejects(plan: &Plan, expr: ExprRef, nulled: &TableSet) -> bool {
    known(plan, expr, nulled).dropped()
}

/// What `expr` evaluates to when every column binding into `nulled` is null.
fn known(plan: &Plan, expr: ExprRef, nulled: &TableSet) -> Known {
    match *plan.expr(expr) {
        Expr::Column(binding) => {
            if nulled.contains(binding.table) {
                Known::Null
            } else {
                Known::Unknown
            }
        }
        Expr::Constant(value) => match plan.value(value) {
            Value::Null => Known::Null,
            Value::Boolean(true) => Known::True,
            Value::Boolean(false) => Known::False,
            _ => Known::Present,
        },
        // A cast of null is null whether or not it is a try cast, and a cast of anything else is a
        // value this has nothing to say about.
        Expr::Cast { input, .. } => match known(plan, input, nulled) {
            Known::Null => Known::Null,
            _ => Known::Unknown,
        },
        Expr::Compare { op, left, right } => {
            compare(op, known(plan, left, nulled), known(plan, right, nulled))
        }
        Expr::Conjunction { op, children } => {
            let parts: Vec<Known> =
                plan.expr_list(children).iter().map(|&part| known(plan, part, nulled)).collect();
            match op {
                ConjunctionOp::And => and(&parts),
                ConjunctionOp::Or => or(&parts),
            }
        }
        Expr::Function { name, args } => function(plan, plan.string(name), args, nulled),
        Expr::Aggregate { .. } | Expr::Case { .. } => Known::Unknown,
    }
}

/// What a comparison of two of these evaluates to.
///
/// The six ordinary comparisons are null when either side is null. The two that are not are the
/// total ones, which is where `IS NULL` and `IS NOT NULL` arrive from the binder: two nulls are not
/// distinct from each other, and a null and a value that is known to be there are.
fn compare(op: CompareOp, left: Known, right: Known) -> Known {
    let null = left == Known::Null || right == Known::Null;
    let both = left == Known::Null && right == Known::Null;
    let one = null && (left.present() || right.present());
    match op {
        CompareOp::NotDistinctFrom if both => Known::True,
        CompareOp::NotDistinctFrom if one => Known::False,
        CompareOp::DistinctFrom if both => Known::False,
        CompareOp::DistinctFrom if one => Known::True,
        CompareOp::NotDistinctFrom | CompareOp::DistinctFrom => Known::Unknown,
        _ if null => Known::Null,
        _ => Known::Unknown,
    }
}

/// What an `AND` over these evaluates to.
///
/// One part being dropped is the whole `AND` being dropped, whatever the other parts are. That is
/// the line the rewrite runs on, since `WHERE a.x = 1 AND b.x = 2` over a left join is the shape
/// people actually write, and reading it as one predicate that says nothing would be reading it as
/// the one thing it does not say.
fn and(parts: &[Known]) -> Known {
    if parts.contains(&Known::False) {
        return Known::False;
    }
    if parts.iter().any(|part| part.dropped()) {
        return Known::Dropped;
    }
    if parts.iter().all(|part| *part == Known::True) { Known::True } else { Known::Unknown }
}

/// What an `OR` over these evaluates to.
///
/// Every part has to be dropped for the `OR` to be, since the row a part threw away is a row another
/// part may keep. The two arms before that one are there for the plans a later pass writes: an `OR`
/// whose parts are all false is false, and one whose parts are all false or null is null, and both
/// of those are worth saying exactly rather than as the two of them together.
fn or(parts: &[Known]) -> Known {
    if parts.contains(&Known::True) {
        return Known::True;
    }
    if parts.iter().all(|part| *part == Known::False) {
        return Known::False;
    }
    if parts.iter().all(|part| *part == Known::False || *part == Known::Null) {
        return Known::Null;
    }
    if parts.iter().all(|part| part.dropped()) { Known::Dropped } else { Known::Unknown }
}

/// What a call evaluates to.
///
/// `not` is the one that has to be spelled out, since it is the only function here that is about
/// three valued logic rather than about values, and it is what a predicate written with `NOT` in
/// front of it becomes.
fn function(plan: &Plan, name: &str, args: Slice, nulled: &TableSet) -> Known {
    let parts: Vec<Known> =
        plan.expr_list(args).iter().map(|&arg| known(plan, arg, nulled)).collect();
    if name == "not" {
        return match parts.first() {
            Some(Known::True) => Known::False,
            Some(Known::False) => Known::True,
            Some(Known::Null) => Known::Null,
            _ => Known::Unknown,
        };
    }
    if TOLERANT.contains(&name) {
        return Known::Unknown;
    }
    if parts.contains(&Known::Null) { Known::Null } else { Known::Unknown }
}

#[cfg(test)]
mod tests {
    use rudb_plan::{JoinKind, Node, Plan};

    use crate::tables::produced;

    /// The kind a join of `kind` with `predicate` over it really is.
    ///
    /// The left side produces table 0 and the right produces table 1, so a predicate naming `#1.0`
    /// is a predicate over the side a left join pads.
    fn narrowed(kind: JoinKind, predicate: &str) -> JoinKind {
        let keyword = kind.keyword();
        let text = format!(
            "\
Filter {predicate}
  Join {keyword} on=[(#0.0::INTEGER = #1.0::INTEGER)::BOOLEAN]
    Get memory.main.t AS a #0 [a::INTEGER]
    Get memory.main.t AS b #1 [a::INTEGER]
"
        );
        let plan =
            Plan::parse(&text).unwrap_or_else(|error| panic!("{text} did not parse: {error}"));
        let Node::Filter { input, predicate } = *plan.node(plan.root()) else {
            panic!("{text} is not a filter over a join");
        };
        let Node::Join { left, right, kind, .. } = *plan.node(input) else {
            panic!("{text} is not a filter over a join");
        };
        let below = (produced(&plan, left), produced(&plan, right));
        super::narrow(&plan, kind, &[predicate], (&below.0, &below.1))
    }

    #[test]
    fn a_comparison_against_a_padded_column_leaves_an_inner_join() {
        let predicate = "(#1.0::INTEGER > 5::INTEGER)::BOOLEAN";
        assert_eq!(narrowed(JoinKind::Left, predicate), JoinKind::Inner);
    }

    #[test]
    fn a_predicate_over_the_kept_side_says_nothing_about_the_padding() {
        let predicate = "(#0.0::INTEGER > 5::INTEGER)::BOOLEAN";
        assert_eq!(narrowed(JoinKind::Left, predicate), JoinKind::Left);
    }

    #[test]
    fn a_predicate_over_both_sides_rejects_when_either_side_of_it_is_padded() {
        // The equality is null as soon as one operand is, so a `WHERE` clause that joins the two
        // tables a second time is a `WHERE` clause that threw away every padded row.
        let predicate = "(#0.1::INTEGER = #1.0::INTEGER)::BOOLEAN";
        assert_eq!(narrowed(JoinKind::Left, predicate), JoinKind::Inner);
    }

    #[test]
    fn is_not_null_rejects_the_padded_row_and_is_null_is_what_asks_for_it() {
        let not_null = "(#1.0::INTEGER IS DISTINCT FROM NULL::\"NULL\")::BOOLEAN";
        assert_eq!(narrowed(JoinKind::Left, not_null), JoinKind::Inner);

        // The anti join written the long way. A padded row is the only row this keeps, so the join
        // that makes them has to stay.
        let is_null = "(#1.0::INTEGER IS NOT DISTINCT FROM NULL::\"NULL\")::BOOLEAN";
        assert_eq!(narrowed(JoinKind::Left, is_null), JoinKind::Left);
    }

    #[test]
    fn not_of_is_null_is_the_same_answer_as_is_not_null() {
        let predicate =
            "not((#1.0::INTEGER IS NOT DISTINCT FROM NULL::\"NULL\")::BOOLEAN)::BOOLEAN";
        assert_eq!(narrowed(JoinKind::Left, predicate), JoinKind::Inner);
    }

    #[test]
    fn an_and_rejects_when_one_part_does_and_an_or_when_every_part_does() {
        let kept = "(#0.0::INTEGER > 5::INTEGER)::BOOLEAN";
        let padded = "(#1.0::INTEGER > 5::INTEGER)::BOOLEAN";

        let and = format!("({kept} AND {padded})::BOOLEAN");
        assert_eq!(narrowed(JoinKind::Left, &and), JoinKind::Inner);

        // One side of an `OR` saying nothing is the whole `OR` saying nothing, since the row the
        // other side rejected may be a row this one keeps.
        let half = format!("({kept} OR {padded})::BOOLEAN");
        assert_eq!(narrowed(JoinKind::Left, &half), JoinKind::Left);

        let both = format!("({padded} OR {padded})::BOOLEAN");
        assert_eq!(narrowed(JoinKind::Left, &both), JoinKind::Inner);
    }

    #[test]
    fn a_function_carries_the_null_up_unless_it_is_one_that_swallows_it() {
        let strict = "(\"+\"(#1.0::INTEGER, 1::INTEGER)::INTEGER > 5::INTEGER)::BOOLEAN";
        assert_eq!(narrowed(JoinKind::Left, strict), JoinKind::Inner);

        // `coalesce(b.a, 0) > 5` is false over a padded row and this does not know it. The list is
        // about what may be assumed, and a function on it may be assumed nothing about.
        let tolerant = "(coalesce(#1.0::INTEGER, 0::INTEGER)::INTEGER > 5::INTEGER)::BOOLEAN";
        assert_eq!(narrowed(JoinKind::Left, tolerant), JoinKind::Left);
    }

    #[test]
    fn a_full_join_loses_one_kind_of_padded_row_at_a_time() {
        // Rejecting on the left takes away the rows that came from an unmatched right row, and what
        // is left is every left row with its match or with nulls.
        let left = "(#0.0::INTEGER > 5::INTEGER)::BOOLEAN";
        assert_eq!(narrowed(JoinKind::Full, left), JoinKind::Left);

        let right = "(#1.0::INTEGER > 5::INTEGER)::BOOLEAN";
        assert_eq!(narrowed(JoinKind::Full, right), JoinKind::Right);

        let both = "(#0.0::INTEGER = #1.0::INTEGER)::BOOLEAN";
        assert_eq!(narrowed(JoinKind::Full, both), JoinKind::Inner);
    }

    #[test]
    fn a_right_join_is_the_left_one_with_the_sides_swapped() {
        let padded = "(#0.0::INTEGER > 5::INTEGER)::BOOLEAN";
        assert_eq!(narrowed(JoinKind::Right, padded), JoinKind::Inner);

        let kept = "(#1.0::INTEGER > 5::INTEGER)::BOOLEAN";
        assert_eq!(narrowed(JoinKind::Right, kept), JoinKind::Right);
    }

    #[test]
    fn a_join_that_pads_nothing_is_handed_back_as_it_was() {
        // A predicate that rejects every padded row there could be, over the four kinds that have
        // none. An inner join is already the answer, a semi and an anti join produce only rows of
        // their left side, a single join carries the at most one row rule that an inner join does
        // not, and a positional join is not about matching at all.
        let predicate = "(#1.0::INTEGER > 5::INTEGER)::BOOLEAN";
        for kind in [
            JoinKind::Inner,
            JoinKind::Semi,
            JoinKind::Anti,
            JoinKind::Single,
            JoinKind::Positional,
        ] {
            assert_eq!(narrowed(kind, predicate), kind);
        }
    }

    #[test]
    fn a_case_is_not_read() {
        // `CASE WHEN b.a IS NULL THEN 0 ELSE 1 END = 1` rejects every padded row and this says so
        // about none of them, which is the direction an unproven answer has to go in.
        let predicate = "\
(CASE WHEN (#1.0::INTEGER IS NOT DISTINCT FROM NULL::\"NULL\")::BOOLEAN \
THEN 0::INTEGER ELSE 1::INTEGER END::INTEGER = 1::INTEGER)::BOOLEAN";
        assert_eq!(narrowed(JoinKind::Left, predicate), JoinKind::Left);
    }

    #[test]
    fn coalesce_is_the_one_function_that_may_not_be_assumed_strict() {
        assert!(super::TOLERANT.contains(&"coalesce"));
    }
}
