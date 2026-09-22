//! The three rewrites a verified relationship licenses, which all delete work rather than speed it
//! up.
//!
//! `spec/stats/07-graph-statistics.md` section 7.3 names two certificates the build writes down
//! about a relationship. Uniqueness: the parent key was read and found distinct, so a child row has
//! at most one parent. Totality: every child row found one, so it has at least one. Together they
//! say a child row has exactly one parent, which is a foreign key that was measured rather than
//! declared, and each of the three rewrites here is a sentence about what a join over such a
//! relationship does to a row set:
//!
//! - **Join elimination.** An inner join to the parent neither drops a child row nor repeats one,
//!   so if nothing above the join reads a parent column the join is doing nothing at all and goes.
//! - **Outer becomes inner.** A `LEFT JOIN` over such a relationship pads no row, so it is an inner
//!   join, which is the cheaper operator and the one a later rewrite is allowed to move.
//! - **Semi becomes nothing.** `EXISTS` over such a relationship is true for every child row, so
//!   the semi join is a filter that keeps everything.
//!
//! The order they are tried in does not matter, because each is a different join kind.
//!
//! # Where this sits in the sequence
//!
//! After the three semi join passes and before the column pruning, and both halves of that are
//! forced. A semi join is not what the binder writes for an `EXISTS`, it is what `crate::semi`
//! leaves behind, so running before those would be running before the node this deletes exists.
//! And deleting a join leaves the key column it was joined on read by nothing, so the pruning has
//! to come after or the plan the optimizer settles on is one more run of the pruning away from
//! settling.
//!
//! What that costs is the join ordering, which has already run by then, so a left join this turns
//! into an inner one is not reordered on the strength of it until the next statement plans the same
//! shape. Moving this above the ordering would trade that for the semi join rewrite, and the semi
//! join rewrite deletes an operator where the ordering only moves one.
//!
//! # Why this is not part of the link join pass
//!
//! Both passes read the same relationships, and there the resemblance stops. The link join pass
//! chooses between two ways of answering a join and needs a size to choose with, which is why it
//! can decline for a reason a reader wants printed. This one deletes an operator on a certificate,
//! which is either licensed or not, and a join it declines is a join the plan already had. So
//! nothing here reports a reason: a query that was not rewritten reads as the query that was
//! written, which is what it ran as.
//!
//! The other difference is what each one needs of the certificates. A link join needs uniqueness
//! and nothing else, because it reads a link. Every rewrite here needs totality too, and totality
//! is the one the file records about the children rather than about the parent.
//!
//! # What this pass will not do
//!
//! It will not eliminate a join whose parent side is anything but a bare scan. A filter over the
//! parent drops parent rows, and a child row whose parent was dropped is a child row the join drops
//! too, which is exactly the row set the certificate promised would not change. The certificate is
//! about the table and the filter is about the query, so no certificate can license this and the
//! shape is refused rather than reasoned about.
//!
//! It will not follow the child's key column up through an outer join. A left join under this one
//! can null the key column of a row it kept, and a null key matches no parent whatever the
//! certificate says. An inner join, a cross product and a filter can drop a row or repeat one and
//! neither invents a null, so the walk down to the child's scan goes through those and stops at
//! anything else.
//!
//! And elimination proper will not fire where the parent's columns would be missed. That is the
//! same question [`crate::link::absorbed`] asks for the link join's extra column, asked about
//! columns going away rather than one arriving, and for the same reason: an operator that reads its
//! input by position or reads all of it sees a change of width as a change of answer.

use rudb_common::Result;
use rudb_common::rules::Rule;
use rudb_plan::{ColumnBinding, CompareOp, Expr, JoinKind, Node, NodeRef, Plan};

use crate::link::{Linked, absorbed, consumers};
use crate::pass::{Context, Pass};
use crate::walk;

/// Deletes the joins a verified relationship says are doing nothing.
#[derive(Debug)]
pub struct JoinElimination;

impl Pass for JoinElimination {
    /// DuckDB's name for the same rewrite, which is already in [`crate::UPSTREAM`] and until now
    /// named nothing here.
    fn name(&self) -> &'static str {
        "join_elimination"
    }

    fn run(&self, plan: &mut Plan, context: &Context) -> Result<()> {
        if context.links().is_empty() || !context.allows(Rule::JoinElimination) {
            return Ok(());
        }
        let consumers = consumers(plan);
        for node in 0..u32::try_from(plan.node_count()).unwrap_or(u32::MAX) {
            rewrite(plan, node, &consumers, context);
        }
        Ok(())
    }
}

/// One join, deleted or narrowed if a certificate says the plan reads the same either way.
fn rewrite(plan: &mut Plan, at: NodeRef, consumers: &[Option<NodeRef>], context: &Context) {
    let Node::Join { left, right, kind, conditions, .. } = *plan.node(at) else {
        return;
    };
    let Some(keys) = equated_pair(plan, conditions) else {
        return;
    };
    // Which side may be the child is the join kind's business, and it is the same rule the link
    // join pass reads: a join that keeps the rows of one input has to have that input as the child,
    // because a certificate about the children is what licenses all of this.
    let sides: &[(NodeRef, NodeRef)] = match kind {
        JoinKind::Inner => &[(left, right), (right, left)],
        JoinKind::Left | JoinKind::Semi => &[(left, right)],
        _ => return,
    };
    for &(child, parent) in sides {
        if !verified(plan, child, parent, keys, context) {
            continue;
        }
        match kind {
            // Every child row keeps exactly one copy of itself, so the join is a no-op on the row
            // set and the only thing it adds is the parent's columns. Where nobody reads those, it
            // adds nothing.
            JoinKind::Inner if unread(plan, parent, at) && absorbed(plan, consumers, at) => {
                stand_in(plan, at, child, consumers);
            }
            // Nothing to check above: a left join over a total relationship emits exactly the rows
            // an inner join over it emits, in the same columns, so this is a change of one field.
            JoinKind::Left => {
                if let Node::Join { kind, .. } = plan.node_mut(at) {
                    *kind = JoinKind::Inner;
                }
            }
            // A semi join's output is its left input's rows and its left input's columns, and this
            // one keeps all of them, so the node is its own input.
            JoinKind::Semi => stand_in(plan, at, child, consumers),
            JoinKind::Inner => return,
            _ => return,
        }
        return;
    }
}

/// Points whatever read the join at the join's child instead.
///
/// The join node is left behind rather than removed, because a plan is an arena and a node nothing
/// points at is a node nothing runs. Taking it out would renumber every node above it, and the
/// numbering is what every other pass in this walk is holding.
fn stand_in(plan: &mut Plan, at: NodeRef, child: NodeRef, consumers: &[Option<NodeRef>]) {
    let Some(above) = consumers.get(at as usize).copied().flatten() else {
        if plan.root() == at {
            plan.set_root(child);
        }
        return;
    };
    let rebuilt: Vec<NodeRef> = plan
        .node(above)
        .children()
        .into_iter()
        .flatten()
        .map(|was| if was == at { child } else { was })
        .collect();
    let mut node = plan.node(above).clone();
    walk::replace_children(&mut node, &rebuilt);
    *plan.node_mut(above) = node;
}

/// Whether the file has verified that every row of the child side has exactly one parent row.
///
/// Both certificates and the shape that makes them apply, which is a parent that is a bare scan and
/// a child key that came out of a base table without passing through anything that could null it.
fn verified(
    plan: &Plan,
    child: NodeRef,
    parent: NodeRef,
    keys: [ColumnBinding; 2],
    context: &Context,
) -> bool {
    let Node::Get { table: parent_name, index: parent_index, columns: projected, .. } =
        *plan.node(parent)
    else {
        return false;
    };
    let [child_key, parent_key] =
        match (keys[0].table == parent_index, keys[1].table == parent_index) {
            (false, true) => [keys[0], keys[1]],
            (true, false) => [keys[1], keys[0]],
            _ => return false,
        };
    let Some(scan) = scan_of(plan, child, child_key.table) else {
        return false;
    };
    let Node::Get { table: child_name, columns: child_columns, .. } = *plan.node(scan) else {
        return false;
    };
    let (Some(child_column), Some(parent_column)) = (
        plan.field_list(child_columns).get(child_key.column as usize),
        plan.field_list(projected).get(parent_key.column as usize),
    ) else {
        return false;
    };
    let relationship = (
        (plan.string(child_name), child_column.name.as_str()),
        (plan.string(parent_name), parent_column.name.as_str()),
    );
    context
        .links()
        .iter()
        .find(|link| link.between(relationship.0, relationship.1))
        .is_some_and(Linked::exactly_one)
}

/// Whether nothing outside the join and its parent side reads a column the parent side produces.
///
/// Asked over every node rather than by walking up from the join, because a pass that ran before
/// this one may have left a node that reads the parent sitting somewhere this walk would not pass
/// through, and a column that is read from an unreachable node is a column this has no business
/// deciding about. Counting it as read costs an elimination and counting it as unread would be a
/// binding pointing at a scan that is no longer in the plan.
fn unread(plan: &Plan, parent: NodeRef, join: NodeRef) -> bool {
    let mut produced = Vec::new();
    indices(plan, parent, &mut produced);
    let mut inside = vec![false; plan.node_count()];
    mark(plan, parent, &mut inside);
    let mut clear = true;
    for node in 0..u32::try_from(plan.node_count()).unwrap_or(u32::MAX) {
        if node == join || inside.get(node as usize).copied().unwrap_or(false) {
            continue;
        }
        walk::node_columns(plan, node, &mut |_, binding| {
            clear &= !produced.contains(&binding.table);
        });
    }
    clear
}

/// The operator numbers a subtree's nodes bind their output columns to.
fn indices(plan: &Plan, at: NodeRef, found: &mut Vec<u32>) {
    if let Some(outputs) = walk::outputs(plan, at) {
        for (binding, _) in outputs {
            if !found.contains(&binding.table) {
                found.push(binding.table);
            }
        }
    }
    for child in plan.node(at).children().into_iter().flatten() {
        indices(plan, child, found);
    }
}

/// Marks every node of a subtree, so that the walk over the whole plan can skip it.
fn mark(plan: &Plan, at: NodeRef, inside: &mut [bool]) {
    if let Some(slot) = inside.get_mut(at as usize) {
        *slot = true;
    }
    for child in plan.node(at).children().into_iter().flatten() {
        mark(plan, child, inside);
    }
}

/// The scan of `index` under `at`, through the operators that cannot put a null in a column.
///
/// A filter drops rows, an inner join drops them and repeats them, and a cross product repeats
/// them. None of the three changes what is in a column of a row it kept, so a key that was a key of
/// the base table before one of them is still one after. Everything else stops the walk, an outer
/// join because it pads, and a projection because the column it produces is its own.
fn scan_of(plan: &Plan, at: NodeRef, index: u32) -> Option<NodeRef> {
    match *plan.node(at) {
        Node::Get { index: found, .. } if found == index => Some(at),
        Node::Filter { input, .. } => scan_of(plan, input, index),
        Node::Join { left, right, kind: JoinKind::Inner, .. }
        | Node::LinkJoin { child: left, parent: right, kind: JoinKind::Inner, .. }
        | Node::CrossProduct { left, right } => {
            scan_of(plan, left, index).or_else(|| scan_of(plan, right, index))
        }
        _ => None,
    }
}

/// The two columns one equality holds equal, when that is what the conditions are.
///
/// The same shape [`crate::link::LinkJoinRewrite`] asks for and for the same reason: one condition,
/// because a second one is a restriction no certificate covers, and two plain columns, because a
/// relationship is between columns.
fn equated_pair(plan: &Plan, conditions: rudb_plan::Slice) -> Option<[ColumnBinding; 2]> {
    let [condition] = plan.expr_list(conditions) else {
        return None;
    };
    let Expr::Compare { op: CompareOp::Equal, left, right } = *plan.expr(*condition) else {
        return None;
    };
    match (plan.expr(left), plan.expr(right)) {
        (&Expr::Column(left), &Expr::Column(right)) => Some([left, right]),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use rudb_common::rules::{Rule, Rules};
    use rudb_plan::Plan;

    use super::JoinElimination;
    use crate::link::Linked;
    use crate::pass::{Context, Pass};

    /// A context holding one relationship with both certificates, unless told otherwise.
    fn context(links: Vec<Linked>) -> Context {
        let mut context = Context::new();
        context.relate(Arc::new(links));
        context
    }

    fn verified() -> Vec<Linked> {
        vec![Linked::verified("orders", "o_custkey", "customer", "c_custkey")]
    }

    /// The join of the two under a projection that reads the child and nothing else, which is the
    /// shape `SELECT o_orderkey FROM orders JOIN customer ON o_custkey = c_custkey` binds to.
    fn joined(kind: &str, projected: &str) -> Plan {
        let text = format!(
            "Project #2 [{projected}]\n  \
             Join {kind} on=[(#0.1::BIGINT = #1.0::BIGINT)::BOOLEAN]\n    \
             Get memory.main.orders AS orders #0 [o_orderkey::BIGINT, o_custkey::BIGINT]\n    \
             Get memory.main.customer AS customer #1 [c_custkey::BIGINT, c_name::VARCHAR]\n"
        );
        Plan::parse(&text).unwrap_or_else(|error| panic!("{text} did not parse: {error}"))
    }

    fn rewritten(plan: &mut Plan, context: &Context) -> String {
        JoinElimination.run(plan, context).expect("the pass does not fail");
        plan.to_string()
    }

    #[test]
    fn an_inner_join_nobody_reads_the_parent_of_is_deleted() {
        let mut plan = joined("INNER", "#0.0::BIGINT AS k");
        let text = rewritten(&mut plan, &context(verified()));
        assert!(!text.contains("Join"), "the join does nothing to the row set: {text}");
        assert!(text.contains("orders"), "and the child is what is left: {text}");
        assert!(!text.contains("customer"), "the parent scan went with it: {text}");
    }

    #[test]
    fn a_parent_column_the_query_projects_keeps_the_join() {
        let mut plan = joined("INNER", "#1.1::VARCHAR AS n");
        let text = rewritten(&mut plan, &context(verified()));
        assert!(text.contains("Join INNER"), "the parent's name is in the answer: {text}");
    }

    #[test]
    fn a_relationship_with_only_the_uniqueness_certificate_licenses_nothing() {
        // A link was built, so a child row has at most one parent, and the rows with none would be
        // dropped by the join. Which rows those are is what the join is for.
        let built = vec![Linked::built("orders", "o_custkey", "customer", "c_custkey")];
        let mut plan = joined("INNER", "#0.0::BIGINT AS k");
        let text = rewritten(&mut plan, &context(built));
        assert!(text.contains("Join INNER"), "at most one is not exactly one: {text}");
    }

    #[test]
    fn a_left_join_over_a_total_relationship_becomes_an_inner_join() {
        let mut plan = joined("LEFT", "#1.1::VARCHAR AS n");
        let text = rewritten(&mut plan, &context(verified()));
        assert!(text.contains("Join INNER"), "no row is padded, so nothing is preserved: {text}");
        // And the parent is still read, because the projection asks for it. Turning the outer join
        // into an inner one is worth doing on its own: the join order pass may move an inner join
        // and may not move an outer one.
        assert!(text.contains("customer"), "the parent's column is still in the answer: {text}");
    }

    #[test]
    fn a_semi_join_over_a_total_relationship_keeps_every_child_row() {
        let text = "Project #2 [#0.0::BIGINT AS k]\n  \
                    Join SEMI on=[(#0.1::BIGINT = #1.0::BIGINT)::BOOLEAN]\n    \
                    Get memory.main.orders AS orders #0 [o_orderkey::BIGINT, o_custkey::BIGINT]\n    \
                    Get memory.main.customer AS customer #1 [c_custkey::BIGINT]\n";
        let mut plan = Plan::parse(text).expect("it parses");
        let text = rewritten(&mut plan, &context(verified()));
        assert!(!text.contains("Join"), "EXISTS is true for every child row: {text}");
        assert!(text.contains("orders"), "which leaves the child: {text}");
    }

    #[test]
    fn the_relationship_has_to_be_the_way_round_it_was_declared() {
        // Customers joined to orders is a relationship in the other direction, and a certificate
        // about the orders side says nothing about how many orders a customer has.
        let backwards = vec![Linked::verified("customer", "c_custkey", "orders", "o_custkey")];
        let mut plan = joined("INNER", "#0.0::BIGINT AS k");
        let text = rewritten(&mut plan, &context(backwards));
        assert!(text.contains("Join INNER"), "the direction is part of the fact: {text}");
    }

    #[test]
    fn a_parent_behind_a_filter_is_not_the_table_the_certificate_is_about() {
        let text = "Project #3 [#0.0::BIGINT AS k]\n  \
                    Join INNER on=[(#0.1::BIGINT = #1.0::BIGINT)::BOOLEAN]\n    \
                    Get memory.main.orders AS orders #0 [o_orderkey::BIGINT, o_custkey::BIGINT]\n    \
                    Filter (#1.0::BIGINT > 5::BIGINT)::BOOLEAN\n      \
                    Get memory.main.customer AS customer #1 [c_custkey::BIGINT]\n";
        let mut plan = Plan::parse(text).expect("it parses");
        let text = rewritten(&mut plan, &context(verified()));
        assert!(text.contains("Join INNER"), "the filter drops child rows too: {text}");
    }

    #[test]
    fn the_rule_turns_the_whole_pass_off() {
        let mut rules = Rules::new();
        rules.set(Rule::JoinElimination, false);
        let mut context = context(verified());
        context.govern(rules);
        let mut plan = joined("INNER", "#0.0::BIGINT AS k");
        let text = rewritten(&mut plan, &context);
        assert!(text.contains("Join INNER"), "the switch is what the per rule table needs: {text}");
    }

    #[test]
    fn running_the_pass_twice_gives_the_same_plan() {
        let context = context(verified());
        let mut plan = joined("INNER", "#0.0::BIGINT AS k");
        let once = rewritten(&mut plan, &context);
        let twice = rewritten(&mut plan, &context);
        assert_eq!(once, twice, "a deleted join stays deleted and nothing else goes");
    }
}
