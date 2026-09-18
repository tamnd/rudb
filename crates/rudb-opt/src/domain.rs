//! Lowering a dependent join by pushing it down to where the correlation stops.
//!
//! The rules in `unnest.rs` each recognise a shape. A correlated equality under a scalar aggregate
//! is one, an existence test over a filter is another, and each of them produces a better plan than
//! anything general would, because knowing the shape is what lets them keep the inner input to one
//! scan and one grouping. What is here is the rule for everything else, and the reason it exists is
//! that the shapes are a list somebody wrote and a query is not obliged to be on it.
//!
//! This is Neumann and Kemper, "Unnesting Arbitrary Queries", BTW 2015, which is what the reference
//! binary implements as well. The dependent join is written as a join against a relation of the
//! distinct values the correlated columns take, which is the domain, and then the domain is pushed
//! down through the correlated side one operator at a time. Every operator it passes carries the
//! domain columns along in its output, so the correlated references below can be rewritten to read
//! them rather than the outer row. The push stops where the correlation does: a subtree that reads
//! no outer column is crossed with the domain and is then an ordinary relation, and there is no
//! dependency left anywhere above it. What comes out is joined back to the outer rows on the domain
//! columns.
//!
//! The point is that the inner side is still evaluated once. It is evaluated once per distinct value
//! of the correlated columns rather than once per outer row, and it is evaluated set at a time
//! rather than one value at a time, which is the difference between this and the per outer row loop
//! the executor deliberately does not have.
//!
//! The comparison that joins the domain back is null safe. An outer row whose correlated column is
//! NULL is one of the values the subquery is asked about, `=` would not match it to the domain row
//! that carries it, and the answer for that row would be a missing row rather than whatever the
//! subquery says about NULL.
//!
//! Not every operator has a rule here. A limit inside a correlated subquery means the limit is per
//! domain value rather than over the whole of the inner side, and the same is true of a top N, so
//! both are left alone rather than pushed through into a different query. A window is the same
//! question again and has an answer, which is that the domain columns join the partition keys, and
//! it is not written yet. A set operation has an answer too, which is that the domain is pushed into
//! every branch and the branches then have to agree on where the domain columns sit, and that is not
//! written yet either. A right or full join is refused because a row of the side that is not there
//! carries no domain value, and the join back would then look for an outer row whose key is NULL.
//! Everything with no rule leaves the dependent join in place and the query is refused, which is the
//! honest end rather than a plan that answers a different question.

use std::collections::HashMap;

use rudb_common::{LogicalType, Value};
use rudb_plan::{
    Arm, BuildSide, ColumnBinding, CompareOp, Expr, ExprRef, JoinKind, Node, NodeRef, Plan, Slice,
    SortKey,
};

use crate::tables::{TableSet, produced};
use crate::walk;

/// One outer column that the right side of a dependent join reads.
struct Key {
    /// Where the column is in the left side's output.
    binding: ColumnBinding,
    /// One of the references to it, which is where its type and its span are read from.
    expr: ExprRef,
}

/// A rewritten subtree, as the operator above it needs to see it.
struct Pushed {
    /// The subtree, with the domain crossed in underneath and the correlation rewritten away.
    node: NodeRef,
    /// Where each key sits in the output, in the order the keys were collected.
    keys: Vec<ColumnBinding>,
    /// Which columns of the original subtree are no longer where they were.
    ///
    /// Only an aggregate fills this in, because adding the domain columns to its grouping moves
    /// its aggregates along its output. Whoever reads those columns is above it and has to be told.
    moved: HashMap<ColumnBinding, ColumnBinding>,
}

/// Lowers a dependent join of any shape the rules below have an answer for.
///
/// `None` when something in the correlated side has no rule, which leaves the dependent join where
/// it was.
pub(crate) fn lower(
    plan: &mut Plan,
    left: NodeRef,
    right: NodeRef,
    kind: JoinKind,
    conditions: Slice,
) -> Option<NodeRef> {
    let outer = produced(plan, left);
    let keys = read_keys(plan, right, &outer);
    if keys.is_empty() {
        return None;
    }

    let group_exprs: Vec<ExprRef> = keys.iter().map(|key| key.expr).collect();
    let groups = plan.add_expr_list(&group_exprs);
    let aggregates = plan.add_expr_list(&[]);
    let index = walk::fresh_index(plan);
    let domain = plan.add_node(Node::Aggregate { input: left, index, groups, aggregates });

    let pushed = push(plan, right, domain, index, &keys, &outer)?;

    let held = plan.expr_list(conditions).to_vec();
    let mut all: Vec<ExprRef> =
        held.into_iter().map(|condition| remap(plan, condition, &pushed.moved)).collect();
    for (key, &carried) in keys.iter().zip(&pushed.keys) {
        let ty = plan.expr_type(key.expr).clone();
        let span = plan.expr_span(key.expr);
        let here = plan.add_expr_at(Expr::Column(key.binding), ty.clone(), span);
        let there = plan.add_expr_at(Expr::Column(carried), ty, span);
        all.push(plan.add_expr_at(
            Expr::Compare { op: CompareOp::NotDistinctFrom, left: here, right: there },
            LogicalType::Boolean,
            span,
        ));
    }
    let conditions = plan.add_expr_list(&all);
    Some(plan.add_node(Node::Join {
        left,
        right: pushed.node,
        kind,
        conditions,
        build: BuildSide::default(),
    }))
}

/// Every outer column the subtree reads, each one once, in the order they were met.
fn read_keys(plan: &Plan, at: NodeRef, outer: &TableSet) -> Vec<Key> {
    let mut keys: Vec<Key> = Vec::new();
    collect_keys(plan, at, outer, &mut keys);
    keys
}

fn collect_keys(plan: &Plan, at: NodeRef, outer: &TableSet, keys: &mut Vec<Key>) {
    walk::node_columns(plan, at, &mut |expr, binding| {
        if outer.contains(binding.table) && !keys.iter().any(|key| key.binding == binding) {
            keys.push(Key { binding, expr });
        }
    });
    for child in plan.node(at).children().into_iter().flatten() {
        collect_keys(plan, child, outer, keys);
    }
}

/// Whether anything in the subtree reads an outer column.
fn correlated(plan: &Plan, at: NodeRef, outer: &TableSet) -> bool {
    let mut yes = false;
    walk::node_columns(plan, at, &mut |_, binding| yes |= outer.contains(binding.table));
    yes || plan
        .node(at)
        .children()
        .into_iter()
        .flatten()
        .any(|child| correlated(plan, child, outer))
}

/// Puts the domain under `at` and rewrites everything that read the outer row to read it instead.
fn push(
    plan: &mut Plan,
    at: NodeRef,
    domain: NodeRef,
    index: u32,
    keys: &[Key],
    outer: &TableSet,
) -> Option<Pushed> {
    if !correlated(plan, at, outer) {
        // The bottom of the walk, and the whole point of it. This subtree asks nothing about the
        // outer row, so one evaluation of it beside every domain value is the same relation the
        // dependent join was asking for one value at a time.
        let node = plan.add_node(Node::CrossProduct { left: domain, right: at });
        let carried = (0..keys.len())
            .map(|position| ColumnBinding::new(index, u32::try_from(position).expect("key count")))
            .collect();
        return Some(Pushed { node, keys: carried, moved: HashMap::new() });
    }

    match *plan.node(at) {
        Node::Filter { input, predicate } => {
            let below = push(plan, input, domain, index, keys, outer)?;
            let map = mapping(keys, &below);
            let predicate = remap(plan, predicate, &map);
            let node = plan.add_node(Node::Filter { input: below.node, predicate });
            Some(Pushed { node, keys: below.keys, moved: below.moved })
        }
        Node::Project { input, index: at_index, exprs, names } => {
            let below = push(plan, input, domain, index, keys, outer)?;
            let map = mapping(keys, &below);
            let held = plan.expr_list(exprs).to_vec();
            let mut projected: Vec<ExprRef> =
                held.into_iter().map(|expr| remap(plan, expr, &map)).collect();
            let mut projected_names = plan.name_list(names).to_vec();
            // The domain columns are added to the output rather than only read here, because the
            // join that puts the rows back beside their outer row is above this and a projection
            // that dropped them would leave it nothing to join on.
            let width = projected.len();
            let mut carried = Vec::new();
            for (position, (key, &binding)) in keys.iter().zip(&below.keys).enumerate() {
                let ty = plan.expr_type(key.expr).clone();
                let span = plan.expr_span(key.expr);
                projected.push(plan.add_expr_at(Expr::Column(binding), ty, span));
                projected_names.push(plan.intern(&format!("__domain_{position}")));
                carried.push(ColumnBinding::new(
                    at_index,
                    u32::try_from(width + position).expect("projection width"),
                ));
            }
            let exprs = plan.add_expr_list(&projected);
            let names = plan.add_name_list(&projected_names);
            let node =
                plan.add_node(Node::Project { input: below.node, index: at_index, exprs, names });
            // A projection's output is its own index, so nothing above it reads what was under it
            // and the map from below is finished with here.
            Some(Pushed { node, keys: carried, moved: HashMap::new() })
        }
        Node::Aggregate { input, index: at_index, groups, aggregates } => {
            let below = push(plan, input, domain, index, keys, outer)?;
            aggregate(plan, below, at_index, groups, aggregates, domain, index, keys)
        }
        Node::Distinct { input, on } => {
            let below = push(plan, input, domain, index, keys, outer)?;
            let map = mapping(keys, &below);
            // A plain `DISTINCT` is over the whole row, and the domain columns are part of the row
            // now, so two rows that came from different domain values are already two rows. A
            // `DISTINCT ON` names its columns and the domain columns have to be named with them.
            let on = if on.is_empty() {
                on
            } else {
                let held = plan.expr_list(on).to_vec();
                let mut kept: Vec<ExprRef> =
                    held.into_iter().map(|expr| remap(plan, expr, &map)).collect();
                for (key, &binding) in keys.iter().zip(&below.keys) {
                    let ty = plan.expr_type(key.expr).clone();
                    let span = plan.expr_span(key.expr);
                    kept.push(plan.add_expr_at(Expr::Column(binding), ty, span));
                }
                plan.add_expr_list(&kept)
            };
            let node = plan.add_node(Node::Distinct { input: below.node, on });
            Some(Pushed { node, keys: below.keys, moved: below.moved })
        }
        Node::Sort { input, keys: order } => {
            let below = push(plan, input, domain, index, keys, outer)?;
            let map = mapping(keys, &below);
            // The domain columns sort first, which keeps each domain value's rows together and in
            // the order the query wrote. One sort of everything rather than a sort per value.
            let mut ordering: Vec<SortKey> = Vec::new();
            for (key, &binding) in keys.iter().zip(&below.keys) {
                let ty = plan.expr_type(key.expr).clone();
                let span = plan.expr_span(key.expr);
                let expr = plan.add_expr_at(Expr::Column(binding), ty, span);
                ordering.push(SortKey { expr, descending: false, nulls_first: false });
            }
            for key in plan.sort_key_list(order).to_vec() {
                let expr = remap(plan, key.expr, &map);
                ordering.push(SortKey { expr, ..key });
            }
            let order = plan.add_sort_keys(&ordering);
            let node = plan.add_node(Node::Sort { input: below.node, keys: order });
            Some(Pushed { node, keys: below.keys, moved: below.moved })
        }
        Node::CrossProduct { left, right } => {
            let empty = plan.add_expr_list(&[]);
            sides(plan, left, right, JoinKind::Inner, empty, domain, index, keys, outer)
        }
        // Inner and left and nothing else. A row of the right side of a right join that matched
        // nothing on the left carries no domain value, so the join back would ask for an outer row
        // whose key is NULL and the row would be lost or attached to the wrong one.
        Node::Join {
            left,
            right,
            kind: kind @ (JoinKind::Inner | JoinKind::Left),
            conditions,
            ..
        } => sides(plan, left, right, kind, conditions, domain, index, keys, outer),
        _ => None,
    }
}

/// Groups by the domain columns as well, and puts back the groups the domain has and the input does
/// not.
///
/// Grouping by the domain columns is what makes this one aggregation rather than one per outer row.
/// An aggregate that was over the whole inner input is now over each domain value's share of it,
/// which is what the correlated query asked for.
///
/// That is the whole of it for a query that was already grouped, because a group with no rows in it
/// is not a row of the answer either way. An ungrouped aggregate is the other case and it is the one
/// the literature calls the count bug. `SELECT count(*) FROM t WHERE false` is 0 and not no rows at
/// all, so a domain value whose share of the input is empty still has an answer, and grouping alone
/// would drop it. The groups that were dropped are put back by joining the domain onto the result
/// from the left, which gives the missing ones a row of nulls, and then every aggregate whose answer
/// over no rows is not null has that answer written in place of the null. `count` is the only one of
/// those rudb has.
#[allow(clippy::too_many_arguments)]
fn aggregate(
    plan: &mut Plan,
    below: Pushed,
    at_index: u32,
    groups: Slice,
    aggregates: Slice,
    domain: NodeRef,
    index: u32,
    keys: &[Key],
) -> Option<Pushed> {
    let map = mapping(keys, &below);
    let held = plan.expr_list(groups).to_vec();
    let was_grouped = !held.is_empty();
    let mut grouped: Vec<ExprRef> = held.into_iter().map(|expr| remap(plan, expr, &map)).collect();
    let held = plan.expr_list(aggregates).to_vec();
    let calls: Vec<ExprRef> = held.into_iter().map(|expr| remap(plan, expr, &map)).collect();
    let width = grouped.len();
    let mut carried = Vec::new();
    for (position, (key, &binding)) in keys.iter().zip(&below.keys).enumerate() {
        let ty = plan.expr_type(key.expr).clone();
        let span = plan.expr_span(key.expr);
        grouped.push(plan.add_expr_at(Expr::Column(binding), ty, span));
        carried.push(ColumnBinding::new(
            at_index,
            u32::try_from(width + position).expect("grouping width"),
        ));
    }

    if was_grouped {
        // An aggregate's output is its groups and then its aggregates, so the ones that were there
        // have moved along by however many keys were added.
        let mut moved = HashMap::new();
        for position in 0..calls.len() {
            let was = u32::try_from(width + position).expect("aggregate width");
            let now = u32::try_from(width + keys.len() + position).expect("aggregate width");
            moved.insert(ColumnBinding::new(at_index, was), ColumnBinding::new(at_index, now));
        }
        let groups = plan.add_expr_list(&grouped);
        let aggregates = plan.add_expr_list(&calls);
        let node = plan.add_node(Node::Aggregate {
            input: below.node,
            index: at_index,
            groups,
            aggregates,
        });
        return Some(Pushed { node, keys: carried, moved });
    }

    // The aggregate itself binds against an index of its own here, because what stands where it
    // stood is the projection at the end, and everything above was written against that.
    let inner = walk::fresh_index(plan);
    let groups = plan.add_expr_list(&grouped);
    let aggregates = plan.add_expr_list(&calls);
    let node =
        plan.add_node(Node::Aggregate { input: below.node, index: inner, groups, aggregates });

    // A marker that says this row came from the aggregate rather than from the padding the left join
    // does, which is what tells a count of no rows apart from a count that has not happened.
    let marker = walk::fresh_index(plan);
    let mut carried_exprs = Vec::new();
    let mut carried_names = Vec::new();
    for (position, key) in keys.iter().enumerate() {
        let ty = plan.expr_type(key.expr).clone();
        let span = plan.expr_span(key.expr);
        let at = u32::try_from(position).expect("key count");
        carried_exprs.push(plan.add_expr_at(Expr::Column(ColumnBinding::new(inner, at)), ty, span));
        carried_names.push(plan.intern(&format!("__domain_{position}")));
    }
    for (position, &call) in calls.iter().enumerate() {
        let ty = plan.expr_type(call).clone();
        let span = plan.expr_span(call);
        let at = u32::try_from(keys.len() + position).expect("aggregate width");
        carried_exprs.push(plan.add_expr_at(Expr::Column(ColumnBinding::new(inner, at)), ty, span));
        carried_names.push(plan.intern(&format!("__aggregate_{position}")));
    }
    let present = plan.add_value(Value::Boolean(true));
    carried_exprs.push(plan.add_expr(Expr::Constant(present), LogicalType::Boolean));
    carried_names.push(plan.intern("__present"));
    let exprs = plan.add_expr_list(&carried_exprs);
    let names = plan.add_name_list(&carried_names);
    let answered = plan.add_node(Node::Project { input: node, index: marker, exprs, names });

    let mut conditions = Vec::new();
    for (position, key) in keys.iter().enumerate() {
        let ty = plan.expr_type(key.expr).clone();
        let span = plan.expr_span(key.expr);
        let at = u32::try_from(position).expect("key count");
        let here = plan.add_expr_at(Expr::Column(ColumnBinding::new(index, at)), ty.clone(), span);
        let there = plan.add_expr_at(Expr::Column(ColumnBinding::new(marker, at)), ty, span);
        conditions.push(plan.add_expr_at(
            Expr::Compare { op: CompareOp::NotDistinctFrom, left: here, right: there },
            LogicalType::Boolean,
            span,
        ));
    }
    let conditions = plan.add_expr_list(&conditions);
    let filled = plan.add_node(Node::Join {
        left: domain,
        right: answered,
        kind: JoinKind::Left,
        conditions,
        build: BuildSide::default(),
    });

    // The aggregates come first and the keys after, which is the output an ungrouped aggregate had,
    // so everything above this reads the column it already read.
    let present = plan.add_expr(
        Expr::Column(ColumnBinding::new(
            marker,
            u32::try_from(keys.len() + calls.len()).expect("projection width"),
        )),
        LogicalType::Boolean,
    );
    let mut repaired = Vec::new();
    let mut repaired_names = Vec::new();
    for (position, &call) in calls.iter().enumerate() {
        let ty = plan.expr_type(call).clone();
        let span = plan.expr_span(call);
        let at = u32::try_from(keys.len() + position).expect("aggregate width");
        let column =
            plan.add_expr_at(Expr::Column(ColumnBinding::new(marker, at)), ty.clone(), span);
        repaired.push(match empty_answer(plan, call) {
            None => column,
            Some(value) => {
                if ty != value.logical_type() {
                    return None;
                }
                let reference = plan.add_value(value);
                let otherwise = plan.add_expr_at(Expr::Constant(reference), ty.clone(), span);
                let arms = plan.add_arms(&[Arm { when: present, then: column }]);
                plan.add_expr_at(Expr::Case { arms, otherwise: Some(otherwise) }, ty, span)
            }
        });
        repaired_names.push(plan.intern(&format!("__aggregate_{position}")));
    }
    let mut repaired_keys = Vec::new();
    for (position, key) in keys.iter().enumerate() {
        let ty = plan.expr_type(key.expr).clone();
        let span = plan.expr_span(key.expr);
        let at = u32::try_from(position).expect("key count");
        repaired.push(plan.add_expr_at(Expr::Column(ColumnBinding::new(index, at)), ty, span));
        repaired_names.push(plan.intern(&format!("__domain_{position}")));
        repaired_keys.push(ColumnBinding::new(
            at_index,
            u32::try_from(calls.len() + position).expect("projection width"),
        ));
    }
    let exprs = plan.add_expr_list(&repaired);
    let names = plan.add_name_list(&repaired_names);
    let node = plan.add_node(Node::Project { input: filled, index: at_index, exprs, names });
    Some(Pushed { node, keys: repaired_keys, moved: HashMap::new() })
}

/// What an aggregate answers over no rows at all, for the ones that do not answer null.
///
/// The two spellings of count and nothing else, of the seven rudb has. `SELECT count(*), sum(x),
/// min(x), max(x), avg(x) FROM t WHERE false` is 0 and then four nulls on the pinned build and on
/// rudb. `count(*)` is held as a call to `count_star` with no arguments and `count(x)` as a call to
/// `count` with one, and both of them answer 0 over no rows.
fn empty_answer(plan: &Plan, call: ExprRef) -> Option<Value> {
    let Expr::Aggregate { name, .. } = *plan.expr(call) else {
        return None;
    };
    matches!(plan.string(name), "count" | "count_star").then_some(Value::BigInt(0))
}

/// Pushes the domain into whichever side of a two input operator is the correlated one.
#[allow(clippy::too_many_arguments)]
fn sides(
    plan: &mut Plan,
    left: NodeRef,
    right: NodeRef,
    kind: JoinKind,
    conditions: Slice,
    domain: NodeRef,
    index: u32,
    keys: &[Key],
    outer: &TableSet,
) -> Option<Pushed> {
    let left_reads = correlated(plan, left, outer);
    let right_reads = correlated(plan, right, outer);
    if left_reads && right_reads {
        let first = push(plan, left, domain, index, keys, outer)?;
        let second = push(plan, right, domain, index, keys, outer)?;
        let mut moved = first.moved.clone();
        moved.extend(second.moved.clone());
        let map = mapping(keys, &first);
        let held = plan.expr_list(conditions).to_vec();
        let mut all: Vec<ExprRef> = held.into_iter().map(|expr| remap(plan, expr, &map)).collect();
        // The two copies of the domain have to be the same row of it, or a value from one side
        // would meet every value from the other.
        for (key, (&here, &there)) in keys.iter().zip(first.keys.iter().zip(&second.keys)) {
            let ty = plan.expr_type(key.expr).clone();
            let span = plan.expr_span(key.expr);
            let one = plan.add_expr_at(Expr::Column(here), ty.clone(), span);
            let other = plan.add_expr_at(Expr::Column(there), ty, span);
            all.push(plan.add_expr_at(
                Expr::Compare { op: CompareOp::NotDistinctFrom, left: one, right: other },
                LogicalType::Boolean,
                span,
            ));
        }
        let conditions = plan.add_expr_list(&all);
        let node = plan.add_node(Node::Join {
            left: first.node,
            right: second.node,
            kind,
            conditions,
            build: BuildSide::default(),
        });
        return Some(Pushed { node, keys: first.keys, moved });
    }
    // A left join whose correlated side is the right one is the case with no answer here, for the
    // reason the caller gives. Everything else carries the domain on the left, including the case
    // where neither side reads the outer row and the condition is what does.
    if right_reads && kind != JoinKind::Inner {
        return None;
    }
    let (correlated_side, other) = if right_reads { (right, left) } else { (left, right) };
    let below = push(plan, correlated_side, domain, index, keys, outer)?;
    let map = mapping(keys, &below);
    let held = plan.expr_list(conditions).to_vec();
    let rewritten: Vec<ExprRef> = held.into_iter().map(|expr| remap(plan, expr, &map)).collect();
    let conditions = plan.add_expr_list(&rewritten);
    let (left, right) = if right_reads { (other, below.node) } else { (below.node, other) };
    let node =
        plan.add_node(Node::Join { left, right, kind, conditions, build: BuildSide::default() });
    Some(Pushed { node, keys: below.keys, moved: below.moved })
}

/// What an operator has to rewrite its own expressions with, given what its input did.
///
/// Two things at once: the outer columns now read from the domain the input carries, and whatever
/// the input moved along its own output.
fn mapping(keys: &[Key], below: &Pushed) -> HashMap<ColumnBinding, ColumnBinding> {
    let mut map = below.moved.clone();
    for (key, &binding) in keys.iter().zip(&below.keys) {
        map.insert(key.binding, binding);
    }
    map
}

/// Rewrites every column reference the map has an entry for.
fn remap(plan: &mut Plan, expr: ExprRef, map: &HashMap<ColumnBinding, ColumnBinding>) -> ExprRef {
    if map.is_empty() {
        return expr;
    }
    if let Expr::Column(binding) = *plan.expr(expr) {
        let Some(&moved) = map.get(&binding) else {
            return expr;
        };
        let ty = plan.expr_type(expr).clone();
        let span = plan.expr_span(expr);
        return plan.add_expr_at(Expr::Column(moved), ty, span);
    }
    walk::rebuild(plan, expr, &mut |plan, child| remap(plan, child, map))
}

#[cfg(test)]
mod tests {
    use crate::unnest;
    use rudb_plan::Plan;

    /// The inner side of a correlated subquery, with one operator put on top of the correlated filter.
    ///
    /// Every test here wants the same two tables and the same correlation and differs only in what it
    /// asks for above them, so the shared part is written once. The filter goes one level in from
    /// whatever the last line of `above` was, which is how the caller says how deep it built.
    fn correlated(above: &str) -> Plan {
        let last = above.lines().next_back().expect("at least one operator above the filter");
        let depth = last.len() - last.trim_start().len() + 2;
        let inner = " ".repeat(depth);
        let deeper = " ".repeat(depth + 2);
        let text = format!(
            "DependentJoin SINGLE on=[]\n  Get memory.main.outer AS o #0 [k::INTEGER]\n{above}{inner}Filter (#1.0::INTEGER = #0.0::INTEGER)::BOOLEAN\n{deeper}Get memory.main.inner AS i #1 [k::INTEGER, value::INTEGER]\n"
        );
        Plan::parse(&text).expect("a correlated plan")
    }

    #[test]
    fn a_distinct_inside_the_subquery_is_pushed_through() {
        let mut plan = correlated("  Distinct on=[]\n    Project #2 [#1.1::INTEGER AS value]\n");
        unnest::lower(&mut plan).expect("unnesting succeeds");
        plan.validate().expect("the rewritten plan is valid");
        let after = plan.to_string();
        assert!(!after.contains("DependentJoin"), "{after}");
        assert!(after.contains("__domain_0"), "{after}");
        assert!(after.contains("CrossProduct"), "{after}");
    }

    #[test]
    fn a_distinct_on_gains_the_domain_columns() {
        let mut plan =
            correlated("  Distinct on=[#2.0::INTEGER]\n    Project #2 [#1.1::INTEGER AS value]\n");
        unnest::lower(&mut plan).expect("unnesting succeeds");
        plan.validate().expect("the rewritten plan is valid");
        let after = plan.to_string();
        assert!(!after.contains("DependentJoin"), "{after}");
        // Two expressions in the list, the one that was asked for and the domain column, because
        // DISTINCT ON inside the subquery is per outer row and not over the whole inner side.
        assert!(after.contains("Distinct on=[#2.0::INTEGER, #2.1::INTEGER]"), "{after}");
    }

    #[test]
    fn a_sort_inside_the_subquery_orders_by_the_domain_first() {
        let mut plan = correlated(
            "  Sort [#2.0::INTEGER ASC NULLS LAST]\n    Project #2 [#1.1::INTEGER AS value]\n",
        );
        unnest::lower(&mut plan).expect("unnesting succeeds");
        plan.validate().expect("the rewritten plan is valid");
        let after = plan.to_string();
        assert!(!after.contains("DependentJoin"), "{after}");
        // The domain column leads, so the rows of one outer row are still next to each other and
        // still in the order that was asked for within that run.
        assert!(
            after.contains("Sort [#2.1::INTEGER ASC NULLS LAST, #2.0::INTEGER ASC NULLS LAST]"),
            "{after}"
        );
    }

    #[test]
    fn a_grouped_aggregate_adds_the_domain_to_the_groups() {
        let mut plan =
            correlated("  Aggregate #2 groups=[#1.1::INTEGER] aggregates=[count_star()::BIGINT]\n");
        unnest::lower(&mut plan).expect("unnesting succeeds");
        plan.validate().expect("the rewritten plan is valid");
        let after = plan.to_string();
        assert!(!after.contains("DependentJoin"), "{after}");
        assert!(after.contains("groups=[#1.1::INTEGER, #3.0::INTEGER]"), "{after}");
        // A group that had no rows was not a row of the answer before either, so there is nothing
        // to repair and no marker is built.
        assert!(!after.contains("__present"), "{after}");
    }

    #[test]
    fn an_ungrouped_count_keeps_its_answer_over_no_rows() {
        let mut plan = correlated("  Aggregate #2 groups=[] aggregates=[count_star()::BIGINT]\n");
        unnest::lower(&mut plan).expect("unnesting succeeds");
        plan.validate().expect("the rewritten plan is valid");
        let after = plan.to_string();
        assert!(!after.contains("DependentJoin"), "{after}");
        // The count bug. Grouping by the domain drops the values whose share of the inner side is
        // empty, the left join puts them back as nulls, and the CASE turns those nulls into the 0
        // that a count over no rows answers.
        assert!(after.contains("Join LEFT"), "{after}");
        assert!(after.contains("TRUE::BOOLEAN AS __present"), "{after}");
        assert!(after.contains("CASE WHEN"), "{after}");
        assert!(after.contains("ELSE 0::BIGINT"), "{after}");
    }

    #[test]
    fn an_ungrouped_sum_is_left_alone_because_null_is_already_its_answer() {
        let mut plan =
            correlated("  Aggregate #2 groups=[] aggregates=[sum(#1.1::INTEGER)::HUGEINT]\n");
        unnest::lower(&mut plan).expect("unnesting succeeds");
        plan.validate().expect("the rewritten plan is valid");
        let after = plan.to_string();
        assert!(!after.contains("DependentJoin"), "{after}");
        assert!(after.contains("Join LEFT"), "{after}");
        assert!(!after.contains("CASE WHEN"), "{after}");
    }

    #[test]
    fn a_limit_inside_the_subquery_is_refused() {
        let mut plan = correlated("  Limit 1 offset 0\n    Project #2 [#1.1::INTEGER AS value]\n");
        unnest::lower(&mut plan).expect("unnesting runs");
        let after = plan.to_string();
        // A limit inside a correlated subquery is per outer row, pushing the domain under it would
        // make it one limit over the whole inner side, and answering a different query is worse
        // than saying no.
        assert!(after.contains("DependentJoin"), "{after}");
    }
}
