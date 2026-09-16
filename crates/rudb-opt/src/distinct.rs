//! Turning `COUNT(DISTINCT x)` into a grouping, so that it runs on the machinery grouping already has.
//!
//! `DISTINCT` inside an aggregate is a grouping wearing a different hat. `COUNT(DISTINCT UserID)`
//! asks how many distinct values a column has, which is what `GROUP BY UserID` answers, and
//! `SELECT g, COUNT(DISTINCT x) ... GROUP BY g` asks the same question once per `g`, which is what
//! `GROUP BY g, x` answers. Written that way the second aggregate counts rows.
//!
//! The reason to write it that way is that the two are not the same speed. The grouped aggregate in
//! `rudb-exec` hashes a vector at a time, splits its table across sixteen radix partitions, merges
//! those partitions in parallel and finishes them on as many threads as there were instances. The
//! `DISTINCT` path next to it keeps one hash set per group per call, fills it a row at a time, and
//! merges the sets by walking them. Every improvement made to grouping since then went to the first
//! one and none of it went to the second, so the cheapest way to make `DISTINCT` fast is to stop
//! having a second implementation of it.
//!
//! Eight of the forty three ClickBench queries count distinct values. This rewrite fires on the
//! general value shapes where a grouped table is cheaper than a set of encoded rows. A single
//! `BIGINT` goes to the fixed integer exchange instead. The string case was once the single worst
//! query in the suite against the pinned DuckDB
//! binary: on ten million rows `SELECT COUNT(DISTINCT SearchPhrase) FROM hits` took 0.39 seconds
//! against DuckDB's 0.09, and it now takes 0.14.
//!
//! DuckDB calls this optimizer `distinct_aggregate_rewrite` and so does this, because
//! `SET disabled_optimizers = 'distinct_aggregate_rewrite'` has to turn off the pass it names.
//!
//! # What it produces
//!
//! ```text
//! Aggregate #1 groups=[g] aggregates=[count(DISTINCT x)]
//!   <input>
//! ```
//!
//! becomes
//!
//! ```text
//! Aggregate #1 groups=[#2.0] aggregates=[count(#2.1)]
//!   Aggregate #2 groups=[g, x] aggregates=[]
//!     <input>
//! ```
//!
//! The outer aggregate keeps the original table index and the original output order, group columns
//! then aggregates, so nothing above it has to be rewritten. The inner one gets a fresh index and
//! produces the group keys followed by the distinct arguments, in that order, which is where the
//! outer one's column numbers come from.
//!
//! Nulls come out the same. A null argument makes a group of its own in the inner aggregate, and
//! `count` skips a null the way it skipped the null that never entered the set. An empty input makes
//! no inner rows, and an ungrouped outer aggregate still answers zero over no rows.
//!
//! # What it refuses
//!
//! An aggregate list that is not all `DISTINCT`. `COUNT(*), COUNT(DISTINCT x)` in one node needs the
//! plain calls to be computed per `(g, x)` and re-aggregated above, which works for `min` and `max`
//! and `sum` and not for `avg`, and which changes `count` into `sum` and with it the result type.
//! That is a rewrite of its own and it is deliberately not this one.
//!
//! `DISTINCT` calls that do not all share the same arguments, for the same reason: each distinct
//! argument list needs its own grouping, and combining them needs a join between the results.
//!
//! A `FILTER (WHERE ...)`, which selects rows before the duplicates are collapsed and so cannot move
//! below the grouping that collapses them.
//!
//! A call whose answer depends on more than the set of values it was given. Every aggregate rudb has
//! that accepts `DISTINCT` is in [`SET_DETERMINED`]; the list is written out rather than assumed so
//! that adding an order dependent aggregate later is a decision somebody makes here.
//!
//! A `COUNT(DISTINCT x)` where `x` is a single `BIGINT`. The execution operator exchanges those
//! integers directly to radix owners, so a staged grouping would add an intermediate result.
//!
//! # Where the win is, measured
//!
//! Ten million ClickBench rows on thirty two threads, best of three, seconds. `alone` is an
//! aggregate with no group key and `grouped` is `GROUP BY RegionID`.
//!
//! | case | argument | before | after | DuckDB |
//! |---|---|---|---|---|
//! | alone | SearchPhrase | 0.37 | 0.15 | 0.10 |
//! | alone | UserID | 0.18 | 0.16 | 0.11 |
//! | grouped | URL | 2.96 | 0.59 | 0.32 |
//! | grouped | SearchPhrase | 0.55 | 0.15 | 0.12 |
//! | grouped | ResolutionWidth | 0.08 | 0.08 | 0.06 |
//! | grouped | UserID | 0.19 | 0.20 | 0.11 |
//!
//! The rewrite trades a set per group for a wider grouping key, so what it is worth depends on which
//! set it is replacing. Against the general one, which keys on an encoded row, it is worth between
//! two and five times. Against the specialised one for a single `BIGINT`, which is already about as
//! cheap as a hash insert gets, it is a wash and the wider key makes it slightly worse. The one place
//! the specialised set loses anyway is an aggregate with no group key, where the old path keeps a
//! single set for the whole query, never partitions it and merges it by walking it.
//!
//! The last row is the one this pass leaves alone, and the reason it is still behind DuckDB is not
//! `DISTINCT`. It is that the inner grouping key is two columns, which is the same thing that makes
//! `GROUP BY WatchID, ClientIP` and `GROUP BY ClientIP, ClientIP - 1, ...` slow, and that is a change
//! to how a multi column key is encoded rather than a change to what an aggregate is.
//!
//! # Rebuilding rather than writing in place
//!
//! Two nodes go where one was and a node has to come after its children in the arena, so there is no
//! slot to write the inner aggregate into. The walk rebuilds the path from the root down to
//! whatever changed, which is what late materialisation does for the same reason.

use rudb_common::{LogicalType, Result};
use rudb_plan::{ColumnBinding, Expr, ExprRef, Node, NodeRef, Plan, Slice};

use crate::pass::{Context, Pass};
use crate::walk;

/// The aggregates whose answer depends only on the set of values they were given.
///
/// All five of the aggregates rudb has that take an argument, which is to say every one that
/// `DISTINCT` can be written inside. `count_star` is not here because `COUNT(DISTINCT *)` is not a
/// thing to write. An aggregate that reported which value arrived first, or how many rows it saw
/// rather than how many values, would not belong here.
pub const SET_DETERMINED: [&str; 5] = ["avg", "count", "max", "min", "sum"];

/// Splits an aggregate with `DISTINCT` calls into a grouping and an aggregate over it.
#[derive(Debug, Clone, Copy)]
pub struct DistinctAggregateRewrite;

impl Pass for DistinctAggregateRewrite {
    fn name(&self) -> &'static str {
        "distinct_aggregate_rewrite"
    }

    fn run(&self, plan: &mut Plan, _context: &Context) -> Result<()> {
        split(plan);
        Ok(())
    }
}

/// Rewrites every aggregate in `plan` whose calls are all `DISTINCT` over the same arguments.
///
/// Rewrites by rebuilding, so the root moves when anything changed. A plan this has already run over
/// is left alone the second time, because what it produces has no `DISTINCT` left in it.
pub fn split(plan: &mut Plan) {
    let mut moved = false;
    let root = walk::restack(plan, plan.root(), &mut moved, &mut stage);
    if moved {
        plan.set_root(root);
    }
}

/// The two stage form of `at` when it is an aggregate this applies to, and nothing when it is not.
fn stage(plan: &mut Plan, at: NodeRef) -> Option<NodeRef> {
    let Node::Aggregate { input, index, groups, aggregates } = *plan.node(at) else { return None };
    let calls = plan.expr_list(aggregates).to_vec();
    let args = shared_arguments(plan, &calls)?;
    let keys = plan.expr_list(groups).to_vec();
    if already_cheap(plan, &args) && (!keys.is_empty() || one_count_distinct(plan, &calls)) {
        return None;
    }

    let mut below = keys.clone();
    below.extend_from_slice(&args);
    let below = plan.add_expr_list(&below);
    let staged = walk::fresh_index(plan);
    let inner = plan.add_node(Node::Aggregate {
        input,
        index: staged,
        groups: below,
        aggregates: Slice::EMPTY,
    });

    // The inner aggregate produces the group keys and then the arguments, which is the order they
    // were put in the list above, so a position there is a column number here. The type comes off
    // the expression the column is produced by, since that is what the inner aggregate's field says.
    let column = &mut |plan: &mut Plan, at: usize, source: ExprRef| {
        let ty = plan.expr_type(source).clone();
        let at = u32::try_from(at).expect("an aggregate with this many expressions cannot bind");
        plan.add_expr(Expr::Column(ColumnBinding::new(staged, at)), ty)
    };
    let outer_keys: Vec<ExprRef> =
        keys.iter().enumerate().map(|(at, &key)| column(plan, at, key)).collect();
    let outer_args: Vec<ExprRef> =
        args.iter().enumerate().map(|(at, &arg)| column(plan, keys.len() + at, arg)).collect();
    let outer_args = plan.add_expr_list(&outer_args);

    let mut outer_calls = Vec::with_capacity(calls.len());
    for &call in &calls {
        let Expr::Aggregate { name, .. } = *plan.expr(call) else {
            return None;
        };
        let ty = plan.expr_type(call).clone();
        let plain = Expr::Aggregate { name, args: outer_args, distinct: false, filter: None };
        outer_calls.push(plan.add_expr(plain, ty));
    }
    let outer_keys = plan.add_expr_list(&outer_keys);
    let outer_calls = plan.add_expr_list(&outer_calls);
    Some(plan.add_node(Node::Aggregate {
        input: inner,
        index,
        groups: outer_keys,
        aggregates: outer_calls,
    }))
}

/// The argument list every call shares, when every call is a `DISTINCT` this rewrite can move.
///
/// Nothing for an empty list, for a call that is not `DISTINCT`, for one that carries a `FILTER`,
/// for one whose answer is not decided by the set of values alone, or for two calls that do not ask
/// about the same expressions. Sameness is structural rather than by arena position, so two
/// `COUNT(DISTINCT UserID)` written out twice are one grouping whether or not the binder shared
/// them.
fn shared_arguments(plan: &Plan, calls: &[ExprRef]) -> Option<Vec<ExprRef>> {
    let mut shared: Option<Vec<ExprRef>> = None;
    for &call in calls {
        let Expr::Aggregate { name, args, distinct, filter } = *plan.expr(call) else {
            return None;
        };
        if !distinct || filter.is_some() || !SET_DETERMINED.contains(&plan.string(name)) {
            return None;
        }
        let args = plan.expr_list(args).to_vec();
        if args.is_empty() {
            return None;
        }
        match &shared {
            None => shared = Some(args),
            Some(first) => {
                if first.len() != args.len() {
                    return None;
                }
                if !first.iter().zip(&args).all(|(&one, &other)| walk::same(plan, one, other)) {
                    return None;
                }
            }
        }
    }
    shared
}

/// Whether execution already has a fixed integer path for these arguments.
///
/// A grouped aggregate over one `BIGINT` uses the inline integer distinct state in `rudb-exec`'s
/// `group.rs`. An ungrouped count exchanges fixed integer records to radix owners. Both avoid the
/// encoded row set this rewrite is meant to replace, and the ungrouped exchange also avoids the
/// intermediate grouped result the rewrite would create. Every other argument shape goes to the
/// general set, which keys on an encoded row and is what the rewrite beats by between two and five
/// times.
///
/// This mirrors a decision made in the operator rather than one made here, which is the honest place
/// for it: the pass is choosing between two implementations and has to know which one it is up
/// against. A release that gives grouping a cheaper multi column key should come back and revisit
/// this choice.
fn already_cheap(plan: &Plan, args: &[ExprRef]) -> bool {
    matches!(args, [only] if plan.expr_type(*only) == &LogicalType::BigInt)
}

/// Whether the original node is one COUNT(DISTINCT ...) call.
fn one_count_distinct(plan: &Plan, calls: &[ExprRef]) -> bool {
    let [call] = calls else { return false };
    matches!(
        *plan.expr(*call),
        Expr::Aggregate { name, distinct: true, filter: None, .. }
            if plan.string(name) == "count"
    )
}

#[cfg(test)]
mod tests {
    use rudb_plan::Plan;

    use super::split;

    /// What the plan a text prints looks like once the pass has run over it.
    fn staged(text: &str) -> String {
        let mut plan =
            Plan::parse(text).unwrap_or_else(|error| panic!("{text} did not parse: {error}"));
        split(&mut plan);
        plan.validate().unwrap_or_else(|error| panic!("{text} did not stay valid: {error}"));
        plan.to_string()
    }

    #[test]
    fn an_ungrouped_count_distinct_becomes_a_grouping_with_a_count_over_it() {
        assert_eq!(
            staged(concat!(
                "Aggregate #1 groups=[] aggregates=[count(DISTINCT #0.0::INTEGER)::BIGINT]\n",
                "  Get memory.main.t AS t #0 [a::INTEGER, b::INTEGER]\n",
            )),
            concat!(
                "Aggregate #1 groups=[] aggregates=[count(#2.0::INTEGER)::BIGINT]\n",
                "  Aggregate #2 groups=[#0.0::INTEGER] aggregates=[]\n",
                "    Get memory.main.t AS t #0 [a::INTEGER, b::INTEGER]\n",
            )
        );
    }

    #[test]
    fn a_grouped_count_distinct_groups_by_the_key_and_the_argument() {
        assert_eq!(
            staged(concat!(
                "Aggregate #1 groups=[#0.0::INTEGER] aggregates=[count(DISTINCT #0.1::INTEGER)::BIGINT]\n",
                "  Get memory.main.t AS t #0 [a::INTEGER, b::INTEGER]\n",
            )),
            concat!(
                "Aggregate #1 groups=[#2.0::INTEGER] aggregates=[count(#2.1::INTEGER)::BIGINT]\n",
                "  Aggregate #2 groups=[#0.0::INTEGER, #0.1::INTEGER] aggregates=[]\n",
                "    Get memory.main.t AS t #0 [a::INTEGER, b::INTEGER]\n",
            )
        );
    }

    #[test]
    fn two_distinct_calls_over_the_same_argument_share_one_grouping() {
        assert_eq!(
            staged(concat!(
                "Aggregate #1 groups=[] aggregates=[count(DISTINCT #0.0::INTEGER)::BIGINT, sum(DISTINCT #0.0::INTEGER)::HUGEINT]\n",
                "  Get memory.main.t AS t #0 [a::INTEGER, b::INTEGER]\n",
            )),
            concat!(
                "Aggregate #1 groups=[] aggregates=[count(#2.0::INTEGER)::BIGINT, sum(#2.0::INTEGER)::HUGEINT]\n",
                "  Aggregate #2 groups=[#0.0::INTEGER] aggregates=[]\n",
                "    Get memory.main.t AS t #0 [a::INTEGER, b::INTEGER]\n",
            )
        );
    }

    #[test]
    fn an_aggregate_with_no_distinct_in_it_is_left_alone() {
        let text = concat!(
            "Aggregate #1 groups=[#0.0::INTEGER] aggregates=[count_star()::BIGINT]\n",
            "  Get memory.main.t AS t #0 [a::INTEGER, b::INTEGER]\n",
        );
        assert_eq!(staged(text), text);
    }

    #[test]
    fn a_plain_call_beside_a_distinct_one_is_left_alone() {
        let text = concat!(
            "Aggregate #1 groups=[] aggregates=[count(DISTINCT #0.0::INTEGER)::BIGINT, count_star()::BIGINT]\n",
            "  Get memory.main.t AS t #0 [a::INTEGER, b::INTEGER]\n",
        );
        assert_eq!(staged(text), text);
    }

    #[test]
    fn two_distinct_calls_over_different_arguments_are_left_alone() {
        let text = concat!(
            "Aggregate #1 groups=[] aggregates=[count(DISTINCT #0.0::INTEGER)::BIGINT, count(DISTINCT #0.1::INTEGER)::BIGINT]\n",
            "  Get memory.main.t AS t #0 [a::INTEGER, b::INTEGER]\n",
        );
        assert_eq!(staged(text), text);
    }

    #[test]
    fn a_distinct_call_with_a_filter_is_left_alone() {
        let text = concat!(
            "Aggregate #1 groups=[] aggregates=[count(DISTINCT #0.0::INTEGER FILTER #0.2::BOOLEAN)::BIGINT]\n",
            "  Get memory.main.t AS t #0 [a::INTEGER, b::INTEGER, c::BOOLEAN]\n",
        );
        assert_eq!(staged(text), text);
    }

    #[test]
    fn a_grouped_count_distinct_over_one_bigint_is_left_alone() {
        let text = concat!(
            "Aggregate #1 groups=[#0.0::INTEGER] aggregates=[count(DISTINCT #0.1::BIGINT)::BIGINT]\n",
            "  Get memory.main.t AS t #0 [a::INTEGER, b::BIGINT]\n",
        );
        assert_eq!(staged(text), text, "the row loop has a set of i64 for exactly this");
    }

    #[test]
    fn an_ungrouped_count_distinct_over_one_bigint_uses_the_radix_operator() {
        let text = concat!(
            "Aggregate #1 groups=[] aggregates=[count(DISTINCT #0.1::BIGINT)::BIGINT]\n",
            "  Get memory.main.t AS t #0 [a::INTEGER, b::BIGINT]\n",
        );
        assert_eq!(staged(text), text, "the execution operator has a fixed integer exchange");
    }

    #[test]
    fn a_grouped_count_distinct_over_two_bigints_is_rewritten() {
        assert_eq!(
            staged(concat!(
                "Aggregate #1 groups=[#0.0::INTEGER] aggregates=[count(DISTINCT #0.1::BIGINT, #0.0::INTEGER)::BIGINT]\n",
                "  Get memory.main.t AS t #0 [a::INTEGER, b::BIGINT]\n",
            )),
            concat!(
                "Aggregate #1 groups=[#2.0::INTEGER] aggregates=[count(#2.1::BIGINT, #2.2::INTEGER)::BIGINT]\n",
                "  Aggregate #2 groups=[#0.0::INTEGER, #0.1::BIGINT, #0.0::INTEGER] aggregates=[]\n",
                "    Get memory.main.t AS t #0 [a::INTEGER, b::BIGINT]\n",
            ),
            "two arguments are an encoded row either way"
        );
    }

    #[test]
    fn running_it_twice_is_running_it_once() {
        let text = concat!(
            "Aggregate #1 groups=[#0.0::INTEGER] aggregates=[count(DISTINCT #0.1::INTEGER)::BIGINT]\n",
            "  Get memory.main.t AS t #0 [a::INTEGER, b::INTEGER]\n",
        );
        let once = staged(text);
        assert_eq!(staged(&once), once);
    }
}
