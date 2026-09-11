//! Column pruning, which is the scan half of projection pushdown.
//!
//! A bound plan reads every column of every table it names, because the binder puts a scan's whole
//! schema in the scan and lets the projection above it throw away what nobody asked for. That is the
//! right thing for a binder to do and the wrong thing to run. `spec/09-optimizer.md` section 9.2
//! calls this pass the difference between 20 GB and 200 MB on ClickBench, and it means it literally:
//! the file is 105 columns wide and the average query in that set names three of them.
//!
//! The pass walks down, collects every column each scan is actually read for, narrows the scan's
//! field list to those, and points the readers at their new positions. Dropping a column moves every
//! column after it up, so the rewrite of the bindings is not optional and is the only part of this
//! that can produce a wrong answer rather than a slow one.
//!
//! A scan that nothing reads a column of prunes to no columns at all, which is `SELECT count(*)`.
//! Both scan operators produce chunks that carry a row count and no vectors for that case, and the
//! Parquet reader in particular then reads no column data whatsoever, which is what makes counting
//! the rows of a file a footer read.

use std::collections::{BTreeSet, HashMap, HashSet};

use rudb_plan::{Arm, ColumnBinding, Expr, ExprRef, Node, NodeRef, Plan, Slice};

/// Narrows every scan in `plan` to the columns something above it reads.
///
/// Rewrites in place. A plan this has already run over is left alone the second time, because a
/// scan whose column list is already what is read of it is not changed.
pub fn prune(plan: &mut Plan) {
    let nodes = reachable(plan);
    let exposed = exposed(plan);
    let exprs = expressions(plan, &nodes);

    let mut read: HashMap<u32, BTreeSet<u32>> = HashMap::new();
    for &expr in &exprs {
        if let Expr::Column(binding) = *plan.expr(expr) {
            read.entry(binding.table).or_default().insert(binding.column);
        }
    }

    // Old position to new, per table index, for the scans that lost a column. A scan that kept all
    // of them is not in here, so the rebinding walk below skips it without having to compare.
    let mut moved: HashMap<u32, Vec<u32>> = HashMap::new();
    for &node in &nodes {
        // A scan whose columns are the query's own output is left alone, because narrowing it
        // would change the answer rather than the work. The binder always puts a projection on
        // top, so this is for the plans that come out of `Plan::parse` in the plan tests.
        if exposed.contains(&node) {
            continue;
        }
        let (index, columns) = match *plan.node(node) {
            Node::Get { index, columns, .. } | Node::TableFunction { index, columns, .. } => {
                (index, columns)
            }
            _ => continue,
        };
        let empty = BTreeSet::new();
        let wanted = read.get(&index).unwrap_or(&empty);
        let held = plan.field_list(columns).len();
        if wanted.len() == held {
            continue;
        }
        let kept: Vec<_> = wanted
            .iter()
            .filter_map(|&at| plan.field_list(columns).get(at as usize).cloned())
            .collect();
        // A binding that points past the end of the scan is a malformed plan, and pruning is not
        // where that gets reported. Leaving the scan alone keeps this pass out of the way of
        // `Plan::validate`, which says so with the node number.
        if kept.len() != wanted.len() {
            continue;
        }
        let mut positions = vec![0; held];
        for (new, &old) in wanted.iter().enumerate() {
            positions[old as usize] = new as u32;
        }
        let narrowed = plan.add_fields(&kept);
        match plan.node_mut(node) {
            Node::Get { columns, .. } | Node::TableFunction { columns, .. } => *columns = narrowed,
            _ => unreachable!("the node was one of these two a moment ago"),
        }
        moved.insert(index, positions);
    }

    if moved.is_empty() {
        return;
    }
    for &expr in &exprs {
        let Expr::Column(binding) = *plan.expr(expr) else { continue };
        let Some(positions) = moved.get(&binding.table) else { continue };
        let to = positions[binding.column as usize];
        plan.rebind(expr, ColumnBinding::new(binding.table, to));
    }
}

/// Every node the root reaches, which is every node a run would touch.
///
/// Not every node in the arena. A rewrite that replaced a node leaves the old one behind, and a
/// column read only by something unreachable is a column nothing reads.
fn reachable(plan: &Plan) -> Vec<NodeRef> {
    let mut found = Vec::new();
    let mut pending = vec![plan.root()];
    while let Some(node) = pending.pop() {
        if found.contains(&node) {
            continue;
        }
        found.push(node);
        pending.extend(plan.node(node).children().into_iter().flatten());
    }
    found
}

/// The nodes whose columns reach the query's output unchanged.
///
/// The root, and then down through every operator that passes its input's columns through. Both
/// sides of a join are in it, since a join's output is both of them. The walk stops at the first
/// operator that introduces columns of its own, which is a projection, an aggregate or a set
/// operation, because from there up the scan's columns are that operator's business and not the
/// answer's.
fn exposed(plan: &Plan) -> HashSet<NodeRef> {
    let mut found = HashSet::new();
    let mut pending = vec![plan.root()];
    while let Some(node) = pending.pop() {
        if !found.insert(node) {
            continue;
        }
        if plan.node(node).table_index().is_some() {
            continue;
        }
        pending.extend(plan.node(node).children().into_iter().flatten());
    }
    found
}

/// Every expression those nodes hold, operands included, each one once.
fn expressions(plan: &Plan, nodes: &[NodeRef]) -> Vec<ExprRef> {
    let mut found = Found::default();
    for &node in nodes {
        match *plan.node(node) {
            Node::Get { .. } | Node::Dummy | Node::SetOp { .. } | Node::CrossProduct { .. } => {}
            Node::Values { rows, .. } => {
                for &row in plan.row_list(rows) {
                    list(plan, row, &mut found);
                }
            }
            Node::TableFunction { args, .. } => list(plan, args, &mut found),
            Node::Filter { predicate, .. } => walk(plan, predicate, &mut found),
            Node::Project { exprs, .. } => list(plan, exprs, &mut found),
            Node::Aggregate { groups, aggregates, .. } => {
                list(plan, groups, &mut found);
                list(plan, aggregates, &mut found);
            }
            Node::Sort { keys, .. } => {
                for key in plan.sort_key_list(keys) {
                    walk(plan, key.expr, &mut found);
                }
            }
            Node::Limit { .. } => {}
            Node::Distinct { on, .. } => list(plan, on, &mut found),
            Node::Join { conditions, .. } => list(plan, conditions, &mut found),
        }
    }
    found.order
}

/// The expressions found so far, and which they are.
///
/// The arena shares operands, so the same expression is reached from as many places as refer to it.
/// The set is what stops the walk going over a shared subtree once per reference, which on a `CASE`
/// with a common condition is the difference between a walk and a blowup.
#[derive(Debug, Default)]
struct Found {
    order: Vec<ExprRef>,
    seen: HashSet<ExprRef>,
}

fn list(plan: &Plan, slice: Slice, found: &mut Found) {
    for &expr in plan.expr_list(slice) {
        walk(plan, expr, found);
    }
}

/// One expression and everything under it.
fn walk(plan: &Plan, expr: ExprRef, found: &mut Found) {
    if !found.seen.insert(expr) {
        return;
    }
    found.order.push(expr);
    match *plan.expr(expr) {
        Expr::Column(_) | Expr::Constant(_) => {}
        Expr::Cast { input, .. } => walk(plan, input, found),
        Expr::Compare { left, right, .. } => {
            walk(plan, left, found);
            walk(plan, right, found);
        }
        Expr::Conjunction { children, .. } => list(plan, children, found),
        Expr::Function { args, .. } => list(plan, args, found),
        Expr::Aggregate { args, filter, .. } => {
            list(plan, args, found);
            if let Some(filter) = filter {
                walk(plan, filter, found);
            }
        }
        Expr::Case { arms, otherwise } => {
            for &Arm { when, then } in plan.arm_list(arms) {
                walk(plan, when, found);
                walk(plan, then, found);
            }
            if let Some(otherwise) = otherwise {
                walk(plan, otherwise, found);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The plan a text prints as after pruning, which is what every assertion here reads.
    fn pruned(text: &str) -> String {
        let mut plan =
            Plan::parse(text).unwrap_or_else(|error| panic!("{text} did not parse: {error}"));
        prune(&mut plan);
        plan.validate().unwrap_or_else(|error| panic!("{text} pruned to a bad plan: {error}"));
        plan.to_string()
    }

    #[test]
    fn a_scan_of_a_column_nobody_reads_loses_it() {
        let before = "Project #1 [#0.0::INTEGER AS a]\n  Get memory.main.t AS t #0 [a::INTEGER, b::VARCHAR]\n";
        let after = "Project #1 [#0.0::INTEGER AS a]\n  Get memory.main.t AS t #0 [a::INTEGER]\n";
        assert_eq!(pruned(before), after);
    }

    #[test]
    fn the_columns_that_stay_are_read_from_where_they_moved_to() {
        // The one that can produce a wrong answer rather than a slow one. `c` was column two and is
        // column zero afterwards, and a reader still pointing at two would read off the end.
        let before = "Project #1 [#0.2::VARCHAR AS c]\n  Get memory.main.t AS t #0 [a::INTEGER, b::VARCHAR, c::VARCHAR]\n";
        let after = "Project #1 [#0.0::VARCHAR AS c]\n  Get memory.main.t AS t #0 [c::VARCHAR]\n";
        assert_eq!(pruned(before), after);
    }

    #[test]
    fn a_column_read_only_by_a_filter_is_kept_and_one_read_by_nothing_is_not() {
        let before = "Project #1 [#0.0::INTEGER AS a]\n  Filter (#0.1::INTEGER > 1::INTEGER)::BOOLEAN\n    Get memory.main.t AS t #0 [a::INTEGER, b::INTEGER, c::INTEGER]\n";
        let after = "Project #1 [#0.0::INTEGER AS a]\n  Filter (#0.1::INTEGER > 1::INTEGER)::BOOLEAN\n    Get memory.main.t AS t #0 [a::INTEGER, b::INTEGER]\n";
        assert_eq!(pruned(before), after);
    }

    #[test]
    fn counting_the_rows_reads_no_columns_at_all() {
        // What makes `SELECT count(*)` over a Parquet file a read of the footer and nothing else.
        let before = "Aggregate #1 groups=[] aggregates=[count_star()::BIGINT]\n  Get memory.main.t AS t #0 [a::INTEGER, b::VARCHAR]\n";
        let after = "Aggregate #1 groups=[] aggregates=[count_star()::BIGINT]\n  Get memory.main.t AS t #0 []\n";
        assert_eq!(pruned(before), after);
    }

    #[test]
    fn a_table_function_is_narrowed_the_same_way_a_table_is() {
        let before = "Aggregate #1 groups=[] aggregates=[count_star()::BIGINT]\n  TableFunction read_parquet args=['f.parquet'::VARCHAR] #0 [a::INTEGER, b::VARCHAR]\n";
        let after = "Aggregate #1 groups=[] aggregates=[count_star()::BIGINT]\n  TableFunction read_parquet args=['f.parquet'::VARCHAR] #0 []\n";
        assert_eq!(pruned(before), after);
    }

    #[test]
    fn each_side_of_a_join_is_narrowed_to_what_that_side_is_read_for() {
        let before = "Project #2 [#0.0::INTEGER AS a]\n  Join INNER on=[(#0.0::INTEGER = #1.1::INTEGER)::BOOLEAN]\n    Get memory.main.t AS t #0 [a::INTEGER, b::INTEGER]\n    Get memory.main.u AS u #1 [x::INTEGER, y::INTEGER]\n";
        let after = "Project #2 [#0.0::INTEGER AS a]\n  Join INNER on=[(#0.0::INTEGER = #1.0::INTEGER)::BOOLEAN]\n    Get memory.main.t AS t #0 [a::INTEGER]\n    Get memory.main.u AS u #1 [y::INTEGER]\n";
        assert_eq!(pruned(before), after);
    }

    #[test]
    fn a_scan_whose_columns_are_the_answer_is_left_alone() {
        // Nothing above it names a column, and narrowing it would change the result rather than the
        // work. The binder never builds this, and `Plan::parse` does.
        let text = "Limit 1 offset 0\n  Get memory.main.t AS t #0 [a::INTEGER, b::VARCHAR]\n";
        assert_eq!(pruned(text), text);
    }

    #[test]
    fn a_scan_that_is_already_narrow_is_not_touched() {
        let text = "Project #1 [#0.0::INTEGER AS a]\n  Get memory.main.t AS t #0 [a::INTEGER]\n";
        assert_eq!(pruned(text), text);
    }

    #[test]
    fn pruning_twice_is_pruning_once() {
        let before = "Project #1 [#0.1::VARCHAR AS b]\n  Get memory.main.t AS t #0 [a::INTEGER, b::VARCHAR]\n";
        let once = pruned(before);
        assert_eq!(pruned(&once), once);
    }
}
