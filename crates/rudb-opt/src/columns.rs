//! Column pruning, which is the scan half of projection pushdown.
//!
//! A bound plan reads every column of every table it names, because the binder puts a scan's whole
//! schema in the scan and lets the projection above it throw away what nobody asked for. That is the
//! right thing for a binder to do and the wrong thing to run. `spec/09-optimizer.md` section 9.2
//! calls this pass the difference between 20 GB and 200 MB on ClickBench, and it means it literally:
//! the file is 105 columns wide and the average query in that set names three of them.
//!
//! The pass walks the plan from the root down, carrying the set of columns each table index is read
//! for. At a scan it narrows the field list to the columns something above it named, and at a
//! projection nothing above it reads the whole of, it drops the expressions nobody asked for.
//! Dropping a column moves every column after it up, so the rewrite of the bindings is not optional
//! and is the only part of this that can produce a wrong answer rather than a slow one.
//!
//! The projection half is what makes a view cost what the file costs. A view expands inline at its
//! reference, so `SELECT count(*) FROM hits` over `CREATE VIEW hits AS SELECT * FROM
//! read_parquet(...)` arrives here as a count over a projection of all one hundred and five columns
//! over a scan of all one hundred and five columns. Narrowing only the scan does nothing there,
//! because the projection above it reads every one. Measured on the real ClickBench partition on
//! server2, that count took 3.39 seconds through the projection and 0.009 seconds without it.
//!
//! A scan that nothing reads a column of prunes to no columns at all, which is `SELECT count(*)`.
//! Both scan operators produce chunks that carry a row count and no vectors for that case, and the
//! Parquet reader in particular then reads no column data whatsoever, which is what makes counting
//! the rows of a file a footer read. A projection prunes to no expressions the same way and for the
//! same reason, and passes the row count of its input through.
//!
//! What it does not narrow is an aggregate, a `VALUES` list, and either side of a set operation. The
//! first two are noted where they are skipped. A set operation lines its two sides up by position
//! rather than binding to them, so narrowing one side without the other would change what the
//! columns line up with, and narrowing both would take a rule that maps the set operation's own read
//! set onto each side. That rule is worth writing and is not written here.

use std::collections::{BTreeSet, HashMap, HashSet};

use rudb_common::Result;
use rudb_plan::{Arm, ColumnBinding, Expr, ExprRef, Node, NodeRef, Plan, Slice};

use crate::pass::{Context, Pass, top_down};

/// Narrows every scan and every interior projection to the columns something above reads.
#[derive(Debug, Clone, Copy)]
pub struct UnusedColumns;

impl Pass for UnusedColumns {
    fn name(&self) -> &'static str {
        "unused_columns"
    }

    fn run(&self, plan: &mut Plan, _context: &Context) -> Result<()> {
        prune(plan);
        Ok(())
    }
}

/// Narrows every scan and every interior projection in `plan` to the columns something above reads.
///
/// Rewrites in place. A plan this has already run over is left alone the second time, because a node
/// whose columns are already what is read of it is not changed.
pub fn prune(plan: &mut Plan) {
    let order = top_down(plan);
    let untouched = untouched(plan, &order);
    // Old position to new, per table index, for the nodes that lost a column. A node that kept all
    // of them is not in here, so the rebinding walk below skips it without having to compare.
    let mut moved: HashMap<u32, Vec<u32>> = HashMap::new();
    let mut read: HashMap<u32, BTreeSet<u32>> = HashMap::new();
    let mut found = Found::default();

    // Parents before children, which is what makes one walk enough. A node is narrowed to the
    // columns everything above it reads, so everything above it has to have been read first.
    for node in order {
        if !untouched.contains(&node) {
            narrow(plan, node, &read, &mut moved);
        }
        let mark = found.order.len();
        expressions(plan, node, &mut found);
        for &expr in &found.order[mark..] {
            if let Expr::Column(binding) = *plan.expr(expr) {
                read.entry(binding.table).or_default().insert(binding.column);
            }
        }
    }

    if moved.is_empty() {
        return;
    }
    for &expr in &found.order {
        let Expr::Column(binding) = *plan.expr(expr) else { continue };
        let Some(positions) = moved.get(&binding.table) else { continue };
        let to = positions[binding.column as usize];
        plan.rebind(expr, ColumnBinding::new(binding.table, to));
    }
}

/// Narrow one node to what `read` says is read of it, recording where its columns moved to.
///
/// A node whose bindings point past the end of what it holds is a malformed plan, and pruning is not
/// where that gets reported. Leaving it alone keeps this pass out of the way of [`Plan::validate`],
/// which says so with the node number.
fn narrow(
    plan: &mut Plan,
    node: NodeRef,
    read: &HashMap<u32, BTreeSet<u32>>,
    moved: &mut HashMap<u32, Vec<u32>>,
) {
    let empty = BTreeSet::new();
    match *plan.node(node) {
        // A `VALUES` list keeps its columns on purpose rather than by omission. The rows are already
        // in the plan, so narrowing one saves reading nothing and would cost a rewrite of every row.
        // An aggregate keeps its own on purpose too: an aggregate nobody reads the result of is a
        // shape the binder does not build, and dropping one would drop whatever it counted.
        Node::Get { index, columns, .. } | Node::TableFunction { index, columns, .. } => {
            let wanted = read.get(&index).unwrap_or(&empty);
            let held = plan.field_list(columns).len();
            if wanted.len() == held {
                return;
            }
            let kept: Vec<_> = wanted
                .iter()
                .filter_map(|&at| plan.field_list(columns).get(at as usize).cloned())
                .collect();
            if kept.len() != wanted.len() {
                return;
            }
            let narrowed = plan.add_fields(&kept);
            match plan.node_mut(node) {
                Node::Get { columns, .. } | Node::TableFunction { columns, .. } => {
                    *columns = narrowed;
                }
                _ => unreachable!("the node was one of these two a moment ago"),
            }
            moved.insert(index, positions(wanted, held));
        }
        Node::Project { index, exprs, names, .. } => {
            let wanted = read.get(&index).unwrap_or(&empty);
            let held = plan.expr_list(exprs).len();
            if wanted.len() == held {
                return;
            }
            let kept: Vec<_> = wanted
                .iter()
                .filter_map(|&at| plan.expr_list(exprs).get(at as usize).copied())
                .collect();
            let labels: Vec<_> = wanted
                .iter()
                .filter_map(|&at| plan.name_list(names).get(at as usize).copied())
                .collect();
            if kept.len() != wanted.len() || labels.len() != wanted.len() {
                return;
            }
            let narrowed = plan.add_expr_list(&kept);
            let renamed = plan.add_name_list(&labels);
            match plan.node_mut(node) {
                Node::Project { exprs, names, .. } => {
                    *exprs = narrowed;
                    *names = renamed;
                }
                _ => unreachable!("the node was a projection a moment ago"),
            }
            moved.insert(index, positions(wanted, held));
        }
        _ => {}
    }
}

/// Where each of `held` columns ends up once everything outside `wanted` is dropped.
///
/// The columns that stay keep the order the node had them in rather than the order the query named
/// them in, which for a scan is the difference between reading a Parquet file forwards and seeking
/// back and forth through it. The entries for the dropped columns are never read, since nothing
/// binds to a column that was dropped for not being bound to.
fn positions(wanted: &BTreeSet<u32>, held: usize) -> Vec<u32> {
    let mut positions = vec![0; held];
    for (new, &old) in wanted.iter().enumerate() {
        positions[old as usize] = new as u32;
    }
    positions
}

/// The nodes this pass leaves alone, for either of the two reasons there are.
///
/// The first is that the node's columns are the query's own output, where narrowing would change
/// the answer rather than the work. That is the root, and then down through every operator that
/// passes its input's columns through. Both sides of a join are in it, since a join's output is both
/// of them. The walk stops at the first operator that introduces columns of its own, because from
/// there up those columns are that operator's business and not the answer's. The binder always puts
/// a projection on top, so the scans this reaches are the ones that come out of [`Plan::parse`] in
/// the plan tests.
///
/// The second is that the node feeds a set operation. Nothing binds to either side of one, because
/// a set operation lines its sides up by position and produces an index of its own, so a pass that
/// went by what is bound would narrow both sides to nothing and answer a `UNION ALL` with no columns
/// at all. Every side of every reachable set operation is in here for that reason and not because of
/// where it sits.
fn untouched(plan: &Plan, order: &[NodeRef]) -> HashSet<NodeRef> {
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
    for &node in order {
        if let Node::SetOp { left, right, .. } = *plan.node(node) {
            found.insert(left);
            found.insert(right);
        }
    }
    found
}

/// Every expression one node holds, operands included, each one once.
fn expressions(plan: &Plan, node: NodeRef, found: &mut Found) {
    match *plan.node(node) {
        Node::Get { .. } | Node::Dummy | Node::SetOp { .. } | Node::CrossProduct { .. } => {}
        Node::Values { rows, .. } => {
            for &row in plan.row_list(rows) {
                list(plan, row, found);
            }
        }
        Node::TableFunction { args, .. } => list(plan, args, found),
        Node::Filter { predicate, .. } => walk(plan, predicate, found),
        Node::Project { exprs, .. } => list(plan, exprs, found),
        Node::Aggregate { groups, aggregates, .. } => {
            list(plan, groups, found);
            list(plan, aggregates, found);
        }
        Node::Sort { keys, .. } => {
            for key in plan.sort_key_list(keys) {
                walk(plan, key.expr, found);
            }
        }
        Node::Limit { .. } => {}
        Node::Distinct { on, .. } => list(plan, on, found),
        Node::Join { conditions, .. } => list(plan, conditions, found),
    }
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

    #[test]
    fn the_columns_that_stay_keep_the_order_the_scan_had_them_in() {
        // Not the order the query named them in. A reader that had to seek backwards through a
        // Parquet file because the plan asked for column 40 before column 3 would read the same
        // bytes in a worse order.
        let before = "Project #1 [#0.3::VARCHAR AS d, #0.1::INTEGER AS b]\n  Get memory.main.t AS t #0 [a::INTEGER, b::INTEGER, c::INTEGER, d::VARCHAR]\n";
        let after = "Project #1 [#0.1::VARCHAR AS d, #0.0::INTEGER AS b]\n  Get memory.main.t AS t #0 [b::INTEGER, d::VARCHAR]\n";
        assert_eq!(pruned(before), after);
    }

    #[test]
    fn a_column_only_a_sort_key_reads_is_kept() {
        // `ORDER BY` on a column the query does not select. It never reaches the output and it is
        // still read, so the walk has to go through the sort keys and not only the projection.
        let before = "Project #1 [#0.0::INTEGER AS a]\n  Sort [#0.2::INTEGER DESC NULLS LAST]\n    Get memory.main.t AS t #0 [a::INTEGER, b::INTEGER, c::INTEGER]\n";
        let after = "Project #1 [#0.0::INTEGER AS a]\n  Sort [#0.1::INTEGER DESC NULLS LAST]\n    Get memory.main.t AS t #0 [a::INTEGER, c::INTEGER]\n";
        assert_eq!(pruned(before), after);
    }

    #[test]
    fn a_column_buried_inside_an_expression_is_found_the_same_as_a_bare_one() {
        // The leaves are what count, however many layers of function call and CASE are on top of
        // them, which is what makes the expression walk recursive rather than a look at the roots.
        let before = "Project #1 [upper(CASE WHEN (#0.2::INTEGER > 3::INTEGER)::BOOLEAN THEN #0.0::VARCHAR ELSE ''::VARCHAR END::VARCHAR)::VARCHAR AS a]\n  Get memory.main.t AS t #0 [a::VARCHAR, b::VARCHAR, c::INTEGER]\n";
        let after = "Project #1 [upper(CASE WHEN (#0.1::INTEGER > 3::INTEGER)::BOOLEAN THEN #0.0::VARCHAR ELSE ''::VARCHAR END::VARCHAR)::VARCHAR AS a]\n  Get memory.main.t AS t #0 [a::VARCHAR, c::INTEGER]\n";
        assert_eq!(pruned(before), after);
    }

    #[test]
    fn a_projection_in_the_middle_loses_the_expressions_nothing_above_it_reads() {
        // The shape a view arrives in, and the one narrowing scans alone does nothing for. The
        // inner projection is the view's `SELECT *`, and until it loses `a` and `c` the scan under
        // it has to keep them, because the projection reads them.
        let before = "Project #2 [#1.1::VARCHAR AS b]\n  Project #1 [#0.0::INTEGER AS a, #0.1::VARCHAR AS b, #0.2::INTEGER AS c]\n    Get memory.main.t AS t #0 [a::INTEGER, b::VARCHAR, c::INTEGER]\n";
        let after = "Project #2 [#1.0::VARCHAR AS b]\n  Project #1 [#0.0::VARCHAR AS b]\n    Get memory.main.t AS t #0 [b::VARCHAR]\n";
        assert_eq!(pruned(before), after);
    }

    #[test]
    fn counting_the_rows_through_a_projection_reads_no_columns_either() {
        // `SELECT count(*) FROM hits` where `hits` is a view over the file. Measured on the real
        // ClickBench partition on server2, this is 3.39 seconds before and 0.009 seconds after.
        let before = "Aggregate #2 groups=[] aggregates=[count_star()::BIGINT]\n  Project #1 [#0.0::INTEGER AS a, #0.1::VARCHAR AS b]\n    Get memory.main.t AS t #0 [a::INTEGER, b::VARCHAR]\n";
        let after = "Aggregate #2 groups=[] aggregates=[count_star()::BIGINT]\n  Project #1 []\n    Get memory.main.t AS t #0 []\n";
        assert_eq!(pruned(before), after);
    }

    #[test]
    fn pruning_a_projection_twice_is_pruning_it_once() {
        let before = "Project #2 [#1.1::VARCHAR AS b]\n  Project #1 [#0.0::INTEGER AS a, #0.1::VARCHAR AS b]\n    Get memory.main.t AS t #0 [a::INTEGER, b::VARCHAR]\n";
        let once = pruned(before);
        assert_eq!(pruned(&once), once);
    }

    #[test]
    fn neither_side_of_a_set_operation_is_narrowed() {
        // A set operation lines its sides up by position and nothing binds to either side's index,
        // so a pass that went by what is bound would narrow both of them to nothing and answer a
        // `UNION ALL` with no columns at all.
        let text = "Aggregate #3 groups=[] aggregates=[count_star()::BIGINT]\n  SetOp UNION ALL #2\n    Get memory.main.t AS t #0 [a::INTEGER]\n    Get memory.main.u AS u #1 [x::INTEGER]\n";
        assert_eq!(pruned(text), text);
    }

    #[test]
    fn a_values_list_keeps_its_columns_even_when_nothing_reads_them() {
        // On purpose rather than by omission. The rows are already in the plan, so narrowing one
        // saves reading nothing and would cost a rewrite of every row.
        let text = "Project #1 [#0.0::BIGINT AS a]\n  Values #0 [a::BIGINT, b::BIGINT] rows=[[1::BIGINT, 2::BIGINT], [3::BIGINT, 4::BIGINT]]\n";
        assert_eq!(pruned(text), text);
    }
}
