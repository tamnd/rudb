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
//! Not every operator has a rule here. Everything with no rule leaves the dependent join in place
//! and the query is refused, which is the honest end rather than a plan that answers a different
//! question.

use std::collections::HashMap;

use rudb_common::{Field, LogicalType, Value};
use rudb_plan::{
    Arm, BuildSide, ColumnBinding, CompareOp, ConjunctionOp, Expr, ExprRef, JoinKind, Node,
    NodeRef, Plan, Slice, SortKey, StrRef, WindowBound, WindowExclude, WindowFrame, WindowUnit,
};

use crate::tables::{TableSet, produced};
use crate::walk;

/// The part of the outer side the domain has to be built from, which is rarely all of it.
///
/// The domain is the distinct values the correlated columns take, and every rule that builds one
/// wrote `Aggregate` straight over the outer side, which is the whole `FROM` list. At the point
/// these rules run the `FROM` list is still a cross product, because nothing has turned the `WHERE`
/// into joins yet, so a subquery correlated on one table was reading the product of all of them.
/// TPC-H q21 correlates on two columns of `lineitem` and its outer side is supplier, lineitem,
/// orders and nation, so the domain was a grouping over two and a quarter quintillion rows to find
/// the distinct values of a column of a six million row table.
///
/// So this walks down into the branch that still has every table the keys come from, and the domain
/// is built there instead. It stops where no single branch has them all, which is where the product
/// is genuinely the thing being grouped.
///
/// The result is a superset of the values the keys take at `left` rather than exactly them, because
/// the branch is what the product drew those rows from and the product can drop rows but cannot
/// invent them. A superset is what the domain is allowed to be. Every rule joins the domain's
/// answer back to the real outer rows on the correlated columns, so a domain value no outer row has
/// carries its answer to nothing, and none of them turn into an extra output row. What a superset
/// must not do is turn one answer into two, and it does not, because the answer is grouped by the
/// domain columns and extra values are extra groups rather than extra rows in a group.
///
/// Which branches [`narrow`] may walk into is [`passes_through`].
///
/// A projection is not one of them, and nothing has to say so: a projection's output is its own
/// table, so a key that reads it does not read the table underneath and the test on the way down
/// refuses the branch on its own.
pub(crate) fn narrow(plan: &Plan, left: NodeRef, keys: &[ColumnBinding]) -> NodeRef {
    let mut at = left;
    loop {
        let sides = match *plan.node(at) {
            Node::CrossProduct { left, right } | Node::Join { left, right, .. } => [left, right],
            _ => return at,
        };
        let found = passes_through(plan, at).iter().map(|&which| sides[which]).find(|&side| {
            let there = produced(plan, side);
            keys.iter().all(|key| there.contains(key.table))
        });
        match found {
            Some(side) => at = side,
            None => return at,
        }
    }
}

/// Which of a node's two branches it passes through as they are, as positions into left and right.
///
/// A branch is one [`narrow`] may walk into only where the node above it leaves that branch's
/// columns alone. A cross product and an inner join do that for both branches: they drop and repeat
/// rows, but every value in a column came from the branch it names. A left, single, semi, anti or
/// mark join does it for its left branch, which it keeps whole and untouched, and does not do it for
/// its right, which it pads with nulls the right branch never held. A right join is the mirror of
/// that. A full join and a positional join pad both sides, so neither branch is one to walk into.
fn passes_through(plan: &Plan, at: NodeRef) -> &'static [usize] {
    match *plan.node(at) {
        Node::CrossProduct { .. } => &[0, 1],
        Node::Join { kind, .. } => match kind {
            JoinKind::Inner => &[0, 1],
            JoinKind::Left
            | JoinKind::Single
            | JoinKind::Semi
            | JoinKind::Anti
            | JoinKind::Mark => &[0],
            JoinKind::Right => &[1],
            JoinKind::Full | JoinKind::Positional => &[],
        },
        _ => &[],
    }
}

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
    let bindings: Vec<ColumnBinding> = keys.iter().map(|key| key.binding).collect();
    let input = narrow(plan, left, &bindings);
    let domain = plan.add_node(Node::Aggregate { input, index, groups, aggregates });

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
        Node::Window { input, index: at_index, partition, order, frame, expressions } => {
            let below = push(plan, input, domain, index, keys, outer)?;
            window(plan, below, at_index, partition, order, frame, expressions, keys)
        }
        Node::Limit { input, count, offset } => {
            let below = push(plan, input, domain, index, keys, outer)?;
            limited(plan, below, None, count, offset, keys)
        }
        Node::TopN { input, keys: order, count, offset } => {
            let below = push(plan, input, domain, index, keys, outer)?;
            limited(plan, below, Some(order), Some(count), offset, keys)
        }
        Node::SetOp { left, right, kind, all, index: at_index } => {
            let width = walk::outputs(plan, left)?.len();
            if width != walk::outputs(plan, right)?.len() {
                return None;
            }
            let one = branch(plan, left, domain, index, keys, outer)?;
            let other = branch(plan, right, domain, index, keys, outer)?;
            let node =
                plan.add_node(Node::SetOp { left: one, right: other, kind, all, index: at_index });
            let carried = (0..keys.len())
                .map(|position| {
                    ColumnBinding::new(
                        at_index,
                        u32::try_from(width + position).expect("branch width"),
                    )
                })
                .collect();
            Some(Pushed { node, keys: carried, moved: HashMap::new() })
        }
        Node::Values { index: at_index, columns, rows } => {
            values(plan, at_index, columns, rows, domain, index, keys)
        }
        Node::TableFunction { index: at_index, function, args, options, settings, columns } => {
            lateral(plan, at_index, function, args, options, settings, columns, domain, index, keys)
        }
        Node::CrossProduct { left, right } => {
            let empty = plan.add_expr_list(&[]);
            sides(plan, left, right, JoinKind::Inner, empty, domain, index, keys, outer)
        }
        Node::Join {
            left,
            right,
            kind:
                kind @ (JoinKind::Inner
                | JoinKind::Left
                | JoinKind::Right
                | JoinKind::Full
                | JoinKind::Single),
            conditions,
            ..
        } => sides(plan, left, right, kind, conditions, domain, index, keys, outer),
        // A positional join is what is left out. It pairs the nth row of one side with the nth row
        // of the other, and crossing either side with the domain changes what the nth row is.
        Node::Join {
            left,
            right,
            kind: kind @ (JoinKind::Semi | JoinKind::Anti | JoinKind::Mark),
            conditions,
            ..
        } => filtering(plan, left, right, kind, conditions, domain, index, keys, outer),
        _ => None,
    }
}

/// A window inside a correlated subquery, which has to be evaluated once per outer row.
///
/// `(SELECT max(rank) FROM (SELECT row_number() OVER (ORDER BY w) AS rank FROM i WHERE i.k = o.k))`
/// numbers the rows of one outer row's matches, starting again at one for the next outer row. The
/// domain makes every outer row's matches arrive in the same relation, so a window left alone here
/// would number across all of them at once and answer a different question.
///
/// The fix is the whole of it: the domain columns partition first. A partition is the unit a window
/// is evaluated over, so partitioning by the domain value is exactly one evaluation per outer row,
/// and the ordering and the frame then apply inside that. It is the same trick the sort rule uses
/// for the same reason, and like the sort it is one pass over everything rather than a pass per
/// value.
///
/// Nothing is moved. A window appends its results to the row it was given rather than replacing it,
/// so the domain columns the input carried are still there above with the bindings they had, which
/// is why `below.keys` goes back out unchanged.
#[allow(clippy::too_many_arguments)]
fn window(
    plan: &mut Plan,
    below: Pushed,
    at_index: u32,
    partition: Slice,
    order: Slice,
    frame: WindowFrame,
    expressions: Slice,
    keys: &[Key],
) -> Option<Pushed> {
    let map = mapping(keys, &below);
    // The domain columns go in front of what the query wrote. Which rows share a partition does not
    // depend on the order, since a partition is a set of columns and not a sort, so this is the
    // same choice the sort rule makes for the reason the sort rule makes it: the operator sorts by
    // this list, and leading with the domain keeps one outer row's rows together.
    let mut divided = Vec::new();
    for (key, &binding) in keys.iter().zip(&below.keys) {
        let ty = plan.expr_type(key.expr).clone();
        let span = plan.expr_span(key.expr);
        divided.push(plan.add_expr_at(Expr::Column(binding), ty, span));
    }
    for expr in plan.expr_list(partition).to_vec() {
        divided.push(remap(plan, expr, &map));
    }
    let partition = plan.add_expr_list(&divided);

    let mut ordering = Vec::new();
    for key in plan.sort_key_list(order).to_vec() {
        let expr = remap(plan, key.expr, &map);
        ordering.push(SortKey { expr, ..key });
    }
    let order = plan.add_sort_keys(&ordering);

    // A frame bound is a constant in every query anybody writes, but it is an expression in the
    // plan and an expression that reads the outer row would be left pointing at a table that is no
    // longer underneath this. Rewriting it costs nothing and not rewriting it is a wrong plan.
    let frame = WindowFrame {
        start: bound(plan, frame.start, &map),
        end: bound(plan, frame.end, &map),
        ..frame
    };

    let held = plan.expr_list(expressions).to_vec();
    let rewritten: Vec<ExprRef> = held.into_iter().map(|expr| remap(plan, expr, &map)).collect();
    let expressions = plan.add_expr_list(&rewritten);

    let node = plan.add_node(Node::Window {
        input: below.node,
        index: at_index,
        partition,
        order,
        frame,
        expressions,
    });
    Some(Pushed { node, keys: below.keys, moved: below.moved })
}

/// One side of a set operation, with the domain pushed into it and its output put back in order.
///
/// A set operation matches its two sides by position, so the domain columns have to come out of
/// both sides in the same places. They do not arrive that way. Where they sit depends on what the
/// side is made of: a projection puts them on the end, the cross product at the bottom of the walk
/// puts them in front, and an aggregate moves the columns that were already there. So each side
/// gets a projection written over it that says where everything is, which is the side's own columns
/// in the order it had them and then the domain columns behind them. Both sides come out the same
/// width with the domain in the same places, and the operation above can go back to matching by
/// position.
///
/// Nothing is added to the operation itself. The domain columns are part of the row now, so two
/// rows that came from different outer rows are two rows, which is what makes `UNION` deduplicate
/// inside one outer row rather than across all of them, and the same for what `EXCEPT` subtracts
/// and what `INTERSECT` keeps.
fn branch(
    plan: &mut Plan,
    at: NodeRef,
    domain: NodeRef,
    index: u32,
    keys: &[Key],
    outer: &TableSet,
) -> Option<NodeRef> {
    let before = walk::outputs(plan, at)?;
    let below = push(plan, at, domain, index, keys, outer)?;
    let span = plan.expr_span(keys.first()?.expr);

    let mut projected = Vec::new();
    let mut named = Vec::new();
    for (position, (binding, ty)) in before.into_iter().enumerate() {
        // An aggregate below is the one that moves a column, because adding the domain to its
        // grouping shifts its aggregates along its output. Everything else leaves a binding where
        // it was, so the map is empty and the lookup costs nothing.
        let moved = below.moved.get(&binding).copied().unwrap_or(binding);
        projected.push(plan.add_expr_at(Expr::Column(moved), ty, span));
        named.push(plan.intern(&format!("__branch_{position}")));
    }
    for (position, (key, &binding)) in keys.iter().zip(&below.keys).enumerate() {
        let ty = plan.expr_type(key.expr).clone();
        projected.push(plan.add_expr_at(Expr::Column(binding), ty, span));
        named.push(plan.intern(&format!("__domain_{position}")));
    }

    let exprs = plan.add_expr_list(&projected);
    let names = plan.add_name_list(&named);
    let at_index = walk::fresh_index(plan);
    Some(plan.add_node(Node::Project { input: below.node, index: at_index, exprs, names }))
}

/// A limit or a top N inside a correlated subquery, which is a limit per outer row.
///
/// `(SELECT w FROM i WHERE i.k = o.k ORDER BY w LIMIT 1)` wants the first row of each outer row's
/// matches, and the domain puts every outer row's matches in one relation, so the operator as
/// written would keep one row out of all of them and answer nothing for everybody else. The limit
/// is per domain value the same way the window was, and it is written with a window for exactly
/// that reason: number the rows inside each domain value and keep the numbers the limit asked for.
///
/// `LIMIT n OFFSET m` becomes `row_number() OVER (PARTITION BY domain) BETWEEN m + 1 AND m + n`,
/// and a top N is the same with its own keys as the window's ordering, which is what makes the
/// numbering agree with what the query wanted ordered. `LIMIT ALL OFFSET m` has no upper bound and
/// writes only the one comparison.
///
/// What this gives up is the top N's own bound. A top N holds `count + offset` rows and throws the
/// rest away as it goes, and a window numbering a partition sees all of it, so this is a sort of
/// everything where the operator it replaces was not. That is the same trade the sort rule makes
/// and it is the one worth making, because the alternative on offer is not a cheaper plan, it is
/// refusing the query.
fn limited(
    plan: &mut Plan,
    below: Pushed,
    order: Option<Slice>,
    count: Option<u64>,
    offset: u64,
    keys: &[Key],
) -> Option<Pushed> {
    // `LIMIT ALL` with no offset keeps every row of every outer row's matches, which is what the
    // relation under it already holds, so there is nothing to number and nothing to drop.
    if count.is_none() && offset == 0 {
        return Some(below);
    }
    let map = mapping(keys, &below);
    let span = plan.expr_span(keys.first()?.expr);

    let mut divided = Vec::new();
    for (key, &binding) in keys.iter().zip(&below.keys) {
        let ty = plan.expr_type(key.expr).clone();
        divided.push(plan.add_expr_at(Expr::Column(binding), ty, span));
    }
    let partition = plan.add_expr_list(&divided);

    // A plain `LIMIT` has no ordering, and that is not an omission. Which rows it keeps is not
    // defined by the query, so any of them will do, and numbering them in whatever order they
    // arrive is the same freedom the operator being replaced already had.
    let mut ordering = Vec::new();
    for key in order.map(|order| plan.sort_key_list(order).to_vec()).unwrap_or_default() {
        let expr = remap(plan, key.expr, &map);
        ordering.push(SortKey { expr, ..key });
    }
    let order = plan.add_sort_keys(&ordering);

    // `row_number` reads no rows but its own position, so the frame it is given cannot change what
    // it answers. This is the one the binder writes for an `OVER` clause with an ordering in it.
    let frame = WindowFrame {
        unit: WindowUnit::Range,
        start: WindowBound::UnboundedPreceding,
        end: WindowBound::CurrentRow,
        exclude: WindowExclude::NoOthers,
    };
    let name = plan.intern("row_number");
    let args = plan.add_expr_list(&[]);
    let call = plan.add_expr_at(
        Expr::Window { name, args, distinct: false, filter: None, ignore_nulls: false },
        LogicalType::BigInt,
        span,
    );
    let expressions = plan.add_expr_list(&[call]);
    let at_index = walk::fresh_index(plan);
    let node = plan.add_node(Node::Window {
        input: below.node,
        index: at_index,
        partition,
        order,
        frame,
        expressions,
    });

    let numbered =
        plan.add_expr_at(Expr::Column(ColumnBinding::new(at_index, 0)), LogicalType::BigInt, span);
    let mut bounds = Vec::new();
    if offset > 0 {
        let at = i64::try_from(offset).ok()?;
        let value = plan.add_value(Value::BigInt(at));
        let right = plan.add_expr_at(Expr::Constant(value), LogicalType::BigInt, span);
        bounds.push(plan.add_expr_at(
            Expr::Compare { op: CompareOp::Greater, left: numbered, right },
            LogicalType::Boolean,
            span,
        ));
    }
    if let Some(count) = count {
        // The offset rows are skipped by being numbered and then dropped, so the upper bound counts
        // from the start of the partition and not from where the query starts reading.
        let at = i64::try_from(offset.saturating_add(count)).ok()?;
        let value = plan.add_value(Value::BigInt(at));
        let right = plan.add_expr_at(Expr::Constant(value), LogicalType::BigInt, span);
        bounds.push(plan.add_expr_at(
            Expr::Compare { op: CompareOp::LessOrEqual, left: numbered, right },
            LogicalType::Boolean,
            span,
        ));
    }
    let predicate = match bounds.len() {
        // Both bounds absent is the early return at the top of this, so there is always one here.
        0 => return None,
        1 => bounds[0],
        _ => {
            let children = plan.add_expr_list(&bounds);
            plan.add_expr_at(
                Expr::Conjunction { op: ConjunctionOp::And, children },
                LogicalType::Boolean,
                span,
            )
        }
    };
    let node = plan.add_node(Node::Filter { input: node, predicate });
    Some(Pushed { node, keys: below.keys, moved: below.moved })
}

/// One end of a frame with its expression rewritten, when it has one.
fn bound(
    plan: &mut Plan,
    at: WindowBound,
    map: &HashMap<ColumnBinding, ColumnBinding>,
) -> WindowBound {
    match at {
        WindowBound::Preceding(expr) => WindowBound::Preceding(remap(plan, expr, map)),
        WindowBound::Following(expr) => WindowBound::Following(remap(plan, expr, map)),
        other => other,
    }
}

/// A `VALUES` whose rows read the outer row, which is what `LATERAL (VALUES (o.k * 3))` is.
///
/// There is nothing underneath a `VALUES` for the domain to be pushed into, so here the domain
/// becomes the input rather than something crossed with one. Each domain value produces the rows the
/// literal wrote, with the outer references inside them reading the domain columns instead of the
/// outer row. One row is that and nothing else, a projection over the domain.
///
/// More than one row needs a way to say which of them an output row is, because a projection
/// produces one row per input row and this has to produce several. The domain is crossed with a
/// `VALUES` of the row numbers, which is a literal relation of n rows that reads nothing, and then
/// each output column is a `CASE` over the number picking that row's expression for it. That is n
/// rows per domain value written with the operators there are, rather than an operator that
/// evaluates a source once per row, which is the thing this whole module exists to avoid.
fn values(
    plan: &mut Plan,
    at_index: u32,
    columns: Slice,
    rows: Slice,
    domain: NodeRef,
    index: u32,
    keys: &[Key],
) -> Option<Pushed> {
    let held: Vec<Vec<ExprRef>> =
        plan.row_list(rows).to_vec().into_iter().map(|row| plan.expr_list(row).to_vec()).collect();
    let fields = plan.field_list(columns).to_vec();
    if held.is_empty() {
        return None;
    }
    let span = plan.expr_span(keys.first()?.expr);
    // The outer references read the domain straight rather than something a lower operator carried,
    // because there is no lower operator. Nothing else is moved for the same reason.
    let mut map = HashMap::new();
    for (position, key) in keys.iter().enumerate() {
        let at = u32::try_from(position).expect("key count");
        map.insert(key.binding, ColumnBinding::new(index, at));
    }

    let (input, chosen) = if held.len() == 1 {
        let row = held.into_iter().next().expect("one row");
        (domain, row.into_iter().map(|expr| remap(plan, expr, &map)).collect::<Vec<_>>())
    } else {
        let counter = LogicalType::Integer;
        // The numbers themselves, made once. The literal relation below is built out of them and
        // the condition each output column asks is a comparison against one of them.
        let count = i32::try_from(held.len()).ok()?;
        let numbers: Vec<ExprRef> = (0..count)
            .map(|at| {
                let value = plan.add_value(Value::Integer(at));
                plan.add_expr_at(Expr::Constant(value), counter.clone(), span)
            })
            .collect();
        let numbered: Vec<Slice> =
            numbers.iter().map(|&expr| plan.add_expr_list(&[expr])).collect();
        let counted = plan.add_rows(&numbered);
        let field = Field { name: "__row".to_string(), ty: counter.clone(), not_null: true };
        let named = plan.add_fields(&[field]);
        let table = walk::fresh_index(plan);
        let source = plan.add_node(Node::Values { index: table, columns: named, rows: counted });
        let input = plan.add_node(Node::CrossProduct { left: domain, right: source });
        let which = plan.add_expr_at(Expr::Column(ColumnBinding::new(table, 0)), counter, span);
        let mut chosen = Vec::new();
        for (column, field) in fields.iter().enumerate() {
            let (last, rest) = held.split_last().expect("more than one row");
            let mut arms = Vec::new();
            for (at, written) in rest.iter().enumerate() {
                let when = plan.add_expr_at(
                    Expr::Compare { op: CompareOp::Equal, left: which, right: numbers[at] },
                    LogicalType::Boolean,
                    span,
                );
                let then = remap(plan, written[column], &map);
                arms.push(Arm { when, then });
            }
            // The last row is the `ELSE` rather than an arm of its own. The number is one of the n
            // by construction, so the condition that would test for it is known true wherever the
            // others are false and writing it would only give the folder something to remove.
            let otherwise = remap(plan, last[column], &map);
            let arms = plan.add_arms(&arms);
            chosen.push(plan.add_expr_at(
                Expr::Case { arms, otherwise: Some(otherwise) },
                field.ty.clone(),
                span,
            ));
        }
        (input, chosen)
    };

    // The domain columns are added to the output for the same reason every other rule here adds
    // them, which is that the join putting these rows back beside their outer row is above this.
    let mut projected = chosen;
    let mut names: Vec<_> = fields.iter().map(|field| plan.intern(&field.name)).collect();
    let width = projected.len();
    let mut carried = Vec::new();
    for (position, key) in keys.iter().enumerate() {
        let ty = plan.expr_type(key.expr).clone();
        let at = u32::try_from(position).expect("key count");
        projected.push(plan.add_expr_at(Expr::Column(ColumnBinding::new(index, at)), ty, span));
        names.push(plan.intern(&format!("__domain_{position}")));
        carried.push(ColumnBinding::new(at_index, u32::try_from(width + position).expect("width")));
    }
    let exprs = plan.add_expr_list(&projected);
    let names = plan.add_name_list(&names);
    let node = plan.add_node(Node::Project { input, index: at_index, exprs, names });
    Some(Pushed { node, keys: carried, moved: HashMap::new() })
}

/// A table function whose arguments read the outer row, which is what `FROM o, range(o.n)` is.
///
/// This is the one operator the push cannot go under. A table function's arguments are what produce
/// its rows rather than something read over rows that already exist, so there is no input below it
/// for the domain to be crossed into, and unlike a `VALUES` there is no projection over the domain
/// that says the same thing either, because how many rows a call makes depends on what the arguments
/// come to.
///
/// So the domain becomes the input and the node becomes a [`Node::LateralFunction`], which is the
/// same call made once per row of what is underneath it. That is one call per distinct value of the
/// correlated columns, which is what every other rule here also gives, and not one call per outer
/// row.
///
/// The domain columns come out of it unmoved. A `LateralFunction` appends the function's columns to
/// the row it was given rather than replacing it, the way a window does, so the domain columns are
/// still where the domain put them and `keys` goes back out reading the domain straight.
#[allow(clippy::too_many_arguments)]
fn lateral(
    plan: &mut Plan,
    at_index: u32,
    function: StrRef,
    args: Slice,
    options: Slice,
    settings: Slice,
    columns: Slice,
    domain: NodeRef,
    index: u32,
    keys: &[Key],
) -> Option<Pushed> {
    // The outer references read the domain straight rather than something a lower operator carried,
    // because there is no lower operator. Nothing else is moved for the same reason.
    let mut map = HashMap::new();
    for (position, key) in keys.iter().enumerate() {
        let at = u32::try_from(position).expect("key count");
        map.insert(key.binding, ColumnBinding::new(index, at));
    }
    let held = plan.expr_list(args).to_vec();
    let rewritten: Vec<ExprRef> = held.into_iter().map(|expr| remap(plan, expr, &map)).collect();
    let args = plan.add_expr_list(&rewritten);
    let node = plan.add_node(Node::LateralFunction {
        input: domain,
        index: at_index,
        function,
        args,
        options,
        settings,
        columns,
    });
    let carried = (0..keys.len())
        .map(|position| ColumnBinding::new(index, u32::try_from(position).expect("key count")))
        .collect();
    Some(Pushed { node, keys: carried, moved: HashMap::new() })
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

/// Pushes the domain into whichever sides of a two input operator have to carry it.
///
/// Two things decide that, and they are not the same thing. A side that reads the outer row has to
/// carry the domain because that is what its references are rewritten to read. A side the join
/// preserves has to carry the domain because a row the join keeps has to say which outer row it
/// belongs to, and a preserved row that matched nothing is told nothing by the side it did not
/// match. A left join preserves its left side and a right join preserves its right side.
///
/// So a right join whose left side is the correlated one still pushes the domain into the right
/// side, even though nothing in there asks about the outer row, because that is the side whose rows
/// all survive. Crossing an uncorrelated side with the domain is what `push` already does when it
/// reaches a subtree that asks nothing, so there is no new case for it here.
///
/// A full join preserves both sides and is not handled, since neither copy of the domain is filled
/// in on every row it produces and the answer is a column neither side has.
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
    // A single join preserves its left side the same way a left join does. It is a left join that
    // also insists the right side hand back at most one row, and that insistence is about the rows
    // rather than about which of them survive, so it belongs here with the left join.
    let keeps_left = matches!(kind, JoinKind::Left | JoinKind::Full | JoinKind::Single);
    let keeps_right = matches!(kind, JoinKind::Right | JoinKind::Full);
    let carry_right = right_reads || keeps_right;
    // Nothing forces the domain on to either side of an inner join whose condition is the only
    // thing that reads the outer row, and it has to be somewhere, so it goes on the left.
    let carry_left = left_reads || keeps_left || !carry_right;

    // A full join is the one rule that has to tell the two copies of the domain apart, so it gets a
    // second one bound against an index of its own. Every other rule here shares the one copy, since
    // a row that reaches it came from one side and the value is the same on both sides of a pairing.
    let (other_domain, other_index) =
        if keeps_left && keeps_right { twin(plan, domain)? } else { (domain, index) };

    let first = if carry_left { Some(push(plan, left, domain, index, keys, outer)?) } else { None };
    let second = if carry_right {
        Some(push(plan, right, other_domain, other_index, keys, outer)?)
    } else {
        None
    };

    let mut moved = HashMap::new();
    for side in [first.as_ref(), second.as_ref()].into_iter().flatten() {
        moved.extend(side.moved.clone());
    }

    // The condition reads the left copy of the domain when there is one, and it does not matter
    // which it reads, because the two are equated below when both are there.
    let carrier = first.as_ref().or(second.as_ref())?;
    let mut map = moved.clone();
    for (key, &binding) in keys.iter().zip(&carrier.keys) {
        map.insert(key.binding, binding);
    }
    let held = plan.expr_list(conditions).to_vec();
    let mut all: Vec<ExprRef> = held.into_iter().map(|expr| remap(plan, expr, &map)).collect();

    if let (Some(one), Some(other)) = (&first, &second) {
        // The two copies of the domain have to be the same row of it, or a value from one side
        // would meet every value from the other.
        for (key, (&here, &there)) in keys.iter().zip(one.keys.iter().zip(&other.keys)) {
            let ty = plan.expr_type(key.expr).clone();
            let span = plan.expr_span(key.expr);
            let mine = plan.add_expr_at(Expr::Column(here), ty.clone(), span);
            let yours = plan.add_expr_at(Expr::Column(there), ty, span);
            all.push(plan.add_expr_at(
                Expr::Compare { op: CompareOp::NotDistinctFrom, left: mine, right: yours },
                LogicalType::Boolean,
                span,
            ));
        }
    }

    let conditions = plan.add_expr_list(&all);
    let node = plan.add_node(Node::Join {
        left: first.as_ref().map_or(left, |one| one.node),
        right: second.as_ref().map_or(right, |other| other.node),
        kind,
        conditions,
        build: BuildSide::default(),
    });

    // Which copy of the domain is filled in on every row the join produces. A right join pads its
    // left side, so the left copy is NULL on the rows that matched nothing and the right copy is
    // the one to carry up. A left join and an inner join preserve their left side or neither side,
    // and the left copy is filled in on both of those. A full join pads both sides and is the one
    // case where neither copy will do.
    if keeps_left && keeps_right {
        let one = first.as_ref()?.keys.clone();
        let other = second.as_ref()?.keys.clone();
        return either(plan, node, &one, &other, keys, moved);
    }
    let carried = if keeps_right { second.as_ref()?.keys.clone() } else { carrier.keys.clone() };
    Some(Pushed { node, keys: carried, moved })
}

/// Pushes the domain into a join whose left input is the subject rather than a side.
///
/// A semi, anti or mark join answers a question about each left row, which is whether the right side
/// has a match for it. The left rows come out, or a column about them does, and the right side's
/// rows are only ever consulted. That is what makes this a different rule from [`sides`] rather than
/// another case in it.
///
/// The left input always carries the domain here, and not because anything in it reads the outer
/// row. Every row this join produces is a left row, so the domain has to be in the left input for
/// there to be a domain column in the output at all, and the join back to the outer rows above has
/// nothing to read otherwise.
///
/// The right input carries one only when something in it reads the outer row. What that buys is the
/// part that makes the rule correct rather than merely typed: a left row now says which outer row it
/// belongs to, so the question asked about it has to be asked against the right rows belonging to the
/// same outer row and not against all of them at once. The domain equality goes into the join
/// condition, and from there the match, the lack of one and the mark's third answer are all decided
/// per outer row the way the dependent join decided them.
///
/// The two copies of the domain have to be told apart for that equality to say anything, so the
/// right input gets a second one bound against an index of its own. That is the same [`twin`] the
/// full join rule uses and for the same reason, which is that a comparison between a column and
/// itself is not a comparison.
#[allow(clippy::too_many_arguments)]
fn filtering(
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
    let first = push(plan, left, domain, index, keys, outer)?;
    let second = if correlated(plan, right, outer) {
        let (other_domain, other_index) = twin(plan, domain)?;
        Some(push(plan, right, other_domain, other_index, keys, outer)?)
    } else {
        None
    };

    let mut map = first.moved.clone();
    if let Some(other) = &second {
        map.extend(other.moved.clone());
    }
    for (key, &binding) in keys.iter().zip(&first.keys) {
        map.insert(key.binding, binding);
    }
    let held = plan.expr_list(conditions).to_vec();
    let mut all: Vec<ExprRef> = held.into_iter().map(|expr| remap(plan, expr, &map)).collect();

    if let Some(other) = &second {
        for (key, (&here, &there)) in keys.iter().zip(first.keys.iter().zip(&other.keys)) {
            let ty = plan.expr_type(key.expr).clone();
            let span = plan.expr_span(key.expr);
            let mine = plan.add_expr_at(Expr::Column(here), ty.clone(), span);
            let yours = plan.add_expr_at(Expr::Column(there), ty, span);
            all.push(plan.add_expr_at(
                Expr::Compare { op: CompareOp::NotDistinctFrom, left: mine, right: yours },
                LogicalType::Boolean,
                span,
            ));
        }
    }

    let conditions = plan.add_expr_list(&all);
    let node = plan.add_node(Node::Join {
        left: first.node,
        right: second.as_ref().map_or(right, |other| other.node),
        kind,
        conditions,
        build: BuildSide::default(),
    });

    // A semi and an anti join produce the left side's columns and nothing else, so a binding the
    // right side moved is not reachable above and reporting it would point whatever read it at a
    // column that is not there. A mark join produces both sides, so both halves are reported.
    let mut moved = first.moved;
    if kind == JoinKind::Mark {
        if let Some(other) = second {
            moved.extend(other.moved);
        }
    }
    Some(Pushed { node, keys: first.keys, moved })
}

/// A second copy of the domain, bound against an index of its own.
///
/// The domain is one node that every side of the push shares, so two sides that both carry it carry
/// it with the same binding and nothing downstream can say which of them it is reading. That is fine
/// everywhere but a full join, where which copy is filled in is exactly the question.
///
/// This is the same relation described twice and not a second relation. The node written here reads
/// the input the first one reads, so what it holds is the same rows. It is evaluated twice, which is
/// the price of being able to name the two copies apart, and it is bounded by the number of distinct
/// outer values rather than by the number of outer rows.
fn twin(plan: &mut Plan, domain: NodeRef) -> Option<(NodeRef, u32)> {
    let Node::Aggregate { input, groups, aggregates, .. } = *plan.node(domain) else {
        return None;
    };
    let index = walk::fresh_index(plan);
    Some((plan.add_node(Node::Aggregate { input, index, groups, aggregates }), index))
}

/// Whichever copy of the domain a full join left filled in, as a column of its own.
///
/// A full join preserves both sides, so both carry the domain and each copy is filled in exactly
/// when the row came from that side. A row that matched has both and they agree, because the join
/// condition equates them. A row that matched nothing has the other side padded with NULLs, so the
/// copy from the side it came from is the one to read. Taking whichever of the two is not NULL
/// answers all three cases, and it answers the outer row whose key really is NULL as well, since
/// then both copies are NULL and NULL is what that row's domain value is.
///
/// This is a column neither side has, so it takes a projection, and a projection rebinds everything
/// under it. Every other column the join produced is listed again so that it is still there, and
/// every binding that moved is reported so that whatever reads it above is told where it went.
fn either(
    plan: &mut Plan,
    node: NodeRef,
    left: &[ColumnBinding],
    right: &[ColumnBinding],
    keys: &[Key],
    moved: HashMap<ColumnBinding, ColumnBinding>,
) -> Option<Pushed> {
    let span = plan.expr_span(keys.first()?.expr);
    let produced = walk::outputs(plan, node)?;
    // Nothing below adds a node from here on, so the index the projection will get is already
    // settled and the bindings written into the map below are the ones it ends up with.
    let at_index = walk::fresh_index(plan);

    let mut exprs = Vec::new();
    let mut names = Vec::new();
    let mut relabel = HashMap::new();
    for (binding, ty) in produced {
        // The two copies of the domain are what the coalesced column replaces, so they are not
        // listed again and nothing above is told where they went, because nothing above reads
        // them by binding. The keys are how they are reached.
        if left.contains(&binding) || right.contains(&binding) {
            continue;
        }
        let position = exprs.len();
        relabel.insert(binding, at(at_index, position));
        exprs.push(plan.add_expr_at(Expr::Column(binding), ty, span));
        names.push(plan.intern(&format!("__kept_{position}")));
    }

    let mut carried = Vec::new();
    for (position, (key, (&here, &there))) in keys.iter().zip(left.iter().zip(right)).enumerate() {
        let ty = plan.expr_type(key.expr).clone();
        let mine = plan.add_expr_at(Expr::Column(here), ty.clone(), span);
        let tested = plan.add_expr_at(Expr::Column(here), ty.clone(), span);
        let yours = plan.add_expr_at(Expr::Column(there), ty.clone(), span);
        let nothing = plan.add_value(Value::Null);
        let absent = plan.add_expr_at(Expr::Constant(nothing), ty.clone(), span);
        // `IS DISTINCT FROM NULL` is the null safe way to ask whether a value is there, and the
        // rule already writes that comparison, so this needs no function to be resolved.
        let filled = plan.add_expr_at(
            Expr::Compare { op: CompareOp::DistinctFrom, left: tested, right: absent },
            LogicalType::Boolean,
            span,
        );
        let arms = plan.add_arms(&[Arm { when: filled, then: mine }]);
        carried.push(at(at_index, exprs.len()));
        exprs.push(plan.add_expr_at(Expr::Case { arms, otherwise: Some(yours) }, ty, span));
        names.push(plan.intern(&format!("__domain_{position}")));
    }

    // The map the operators above need is from where a column was in the subtree they were written
    // against to where it is now, and what is in hand is two halves of that. A column nothing moved
    // has the same binding in both halves, so starting from the relabelling and then following each
    // move through it composes the two.
    let mut told = relabel.clone();
    for (before, between) in moved {
        if let Some(&now) = relabel.get(&between) {
            told.insert(before, now);
        }
    }

    let exprs = plan.add_expr_list(&exprs);
    let names = plan.add_name_list(&names);
    let node = plan.add_node(Node::Project { input: node, index: at_index, exprs, names });
    Some(Pushed { node, keys: carried, moved: told })
}

/// A binding at a position that came from counting columns rather than from the plan.
fn at(index: u32, position: usize) -> ColumnBinding {
    ColumnBinding::new(index, u32::try_from(position).expect("a column count fits in a u32"))
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
pub(crate) fn remap(
    plan: &mut Plan,
    expr: ExprRef,
    map: &HashMap<ColumnBinding, ColumnBinding>,
) -> ExprRef {
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

    /// A frame that says nothing, which is what a window with no `OVER` clause contents gets.
    const FRAME: &str = "frame=RANGE UNBOUNDED PRECEDING TO CURRENT ROW EXCLUDE NO OTHERS";

    #[test]
    fn a_window_inside_the_subquery_partitions_by_the_domain() {
        let mut plan = correlated(&format!(
            "  Window #2 partition=[] order=[#1.1::INTEGER ASC NULLS LAST] {FRAME} expressions=[row_number()::BIGINT]\n"
        ));
        unnest::lower(&mut plan).expect("unnesting succeeds");
        plan.validate().expect("the rewritten plan is valid");
        let after = plan.to_string();
        assert!(!after.contains("DependentJoin"), "{after}");
        // A window with nothing to partition by across the whole inner side would number every
        // outer row's matches in one run. Partitioning by the domain is one run per outer row.
        assert!(after.contains("partition=[#3.0::INTEGER]"), "{after}");
    }

    #[test]
    fn a_window_that_already_partitions_puts_the_domain_in_front() {
        let mut plan = correlated(&format!(
            "  Window #2 partition=[#1.1::INTEGER] order=[] {FRAME} expressions=[rank()::BIGINT]\n"
        ));
        unnest::lower(&mut plan).expect("unnesting succeeds");
        plan.validate().expect("the rewritten plan is valid");
        let after = plan.to_string();
        assert!(!after.contains("DependentJoin"), "{after}");
        // In front rather than behind. Behind would put rows of two outer rows in one partition
        // whenever they agreed on the column the query wrote, which is the wrong answer and not a
        // slower one.
        assert!(after.contains("partition=[#3.0::INTEGER, #1.1::INTEGER]"), "{after}");
    }

    #[test]
    fn a_top_n_inside_the_subquery_becomes_a_row_number_per_domain_value() {
        let mut plan = correlated("  TopN 1 offset 0 [#1.1::INTEGER ASC NULLS LAST]\n");
        unnest::lower(&mut plan).expect("unnesting succeeds");
        plan.validate().expect("the rewritten plan is valid");
        let after = plan.to_string();
        assert!(!after.contains("DependentJoin"), "{after}");
        // The operator is gone and what replaced it numbers inside one outer row's matches and
        // keeps the first, rather than keeping one row out of every outer row's matches together.
        assert!(!after.contains("TopN"), "{after}");
        assert!(after.contains("row_number()"), "{after}");
        assert!(after.contains("partition=[#2.0::INTEGER]"), "{after}");
        assert!(after.contains("<= 1::BIGINT"), "{after}");
    }

    #[test]
    fn an_offset_is_the_lower_bound_and_the_count_is_still_measured_from_the_start() {
        let mut plan = correlated("  Limit 2 offset 3\n");
        unnest::lower(&mut plan).expect("unnesting succeeds");
        plan.validate().expect("the rewritten plan is valid");
        let after = plan.to_string();
        assert!(!after.contains("DependentJoin"), "{after}");
        // Rows four and five of each outer row's matches. The upper bound is five and not two,
        // because the skipped rows are numbered before they are dropped.
        assert!(after.contains("> 3::BIGINT"), "{after}");
        assert!(after.contains("<= 5::BIGINT"), "{after}");
    }

    #[test]
    fn a_plain_limit_numbers_the_rows_in_whatever_order_they_arrive() {
        let mut plan = correlated("  Limit 1 offset 0\n");
        unnest::lower(&mut plan).expect("unnesting succeeds");
        plan.validate().expect("the rewritten plan is valid");
        let after = plan.to_string();
        assert!(!after.contains("DependentJoin"), "{after}");
        // No ordering, because `LIMIT` without `ORDER BY` does not say which rows it keeps and the
        // operator being replaced did not say either.
        assert!(after.contains("order=[] "), "{after}");
        assert!(after.contains("<= 1::BIGINT"), "{after}");
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
    fn a_limit_over_a_projection_reads_the_domain_the_projection_carried_up() {
        let mut plan = correlated("  Limit 1 offset 0\n    Project #2 [#1.1::INTEGER AS value]\n");
        unnest::lower(&mut plan).expect("unnesting succeeds");
        plan.validate().expect("the rewritten plan is valid");
        let after = plan.to_string();
        assert!(!after.contains("DependentJoin"), "{after}");
        // The projection between the two is the case worth having, since the limit partitions by a
        // binding the projection produced and not by the one the table underneath it has.
        assert!(after.contains("partition=[#2.1::INTEGER]"), "{after}");
    }

    /// A `VALUES` on the right of a dependent join, which is what `LATERAL (VALUES ...)` binds to.
    ///
    /// Nothing under it and no inner table, so it does not fit the shape `correlated` builds.
    fn correlated_values(rows: &str) -> Plan {
        let text = format!(
            "DependentJoin INNER on=[]\n  Get memory.main.outer AS o #0 [k::INTEGER]\n  Values #1 [col0::INTEGER] rows=[{rows}]\n"
        );
        Plan::parse(&text).expect("a correlated plan")
    }

    #[test]
    fn one_values_row_reading_the_outer_row_becomes_a_projection_over_the_domain() {
        let mut plan = correlated_values("[\"*\"(#0.0::INTEGER, 3::INTEGER)::INTEGER]");
        unnest::lower(&mut plan).expect("unnesting succeeds");
        plan.validate().expect("the rewritten plan is valid");
        let after = plan.to_string();
        assert!(!after.contains("DependentJoin"), "{after}");
        // The domain is the input rather than something crossed with one, because there is nothing
        // under a VALUES for it to be pushed into, and one row needs nothing to choose between.
        assert!(!after.contains("CrossProduct"), "{after}");
        assert!(!after.contains("CASE WHEN"), "{after}");
        assert!(after.contains("__domain_0"), "{after}");
    }

    #[test]
    fn several_values_rows_are_chosen_between_by_a_row_number() {
        let mut plan = correlated_values(
            "[\"*\"(#0.0::INTEGER, 3::INTEGER)::INTEGER], [\"+\"(#0.0::INTEGER, 1::INTEGER)::INTEGER]",
        );
        unnest::lower(&mut plan).expect("unnesting succeeds");
        plan.validate().expect("the rewritten plan is valid");
        let after = plan.to_string();
        assert!(!after.contains("DependentJoin"), "{after}");
        // Two rows per domain value, so the domain is crossed with a literal relation of the row
        // numbers and each output column asks which row it is. The last row is the ELSE.
        assert!(
            after.contains("Values #3 [__row::INTEGER] rows=[[0::INTEGER], [1::INTEGER]]"),
            "{after}"
        );
        assert!(after.contains("CASE WHEN (#3.0::INTEGER = 0::INTEGER)"), "{after}");
        assert!(after.contains("ELSE \"+\"(#2.0::INTEGER, 1::INTEGER)::INTEGER"), "{after}");
    }

    /// A set operation on the right of a dependent join, with whatever is written as its right side.
    ///
    /// Two sides means the shared fixture cannot build it, since that one puts the filter under the
    /// last line it was given and a set operation has two last lines.
    fn correlated_set(kind: &str, right: &str) -> Plan {
        let text = format!(
            "DependentJoin SINGLE on=[]\n  Get memory.main.outer AS o #0 [k::INTEGER]\n  SetOp {kind} #4\n    Project #2 [#1.1::INTEGER AS value]\n      Filter (#1.0::INTEGER = #0.0::INTEGER)::BOOLEAN\n        Get memory.main.inner AS i #1 [k::INTEGER, value::INTEGER]\n{right}"
        );
        Plan::parse(&text).expect("a correlated set operation")
    }

    /// A right side that asks nothing about the outer row, which is the common way to write one.
    const PLAIN: &str = "    Project #3 [#5.1::INTEGER AS value]\n      Get memory.main.other AS u #5 [k::INTEGER, value::INTEGER]\n";

    /// A right side correlated the same way the left one is.
    const ALSO: &str = "    Project #3 [#5.1::INTEGER AS value]\n      Filter (#5.0::INTEGER = #0.0::INTEGER)::BOOLEAN\n        Get memory.main.other AS u #5 [k::INTEGER, value::INTEGER]\n";

    #[test]
    fn a_union_inside_the_subquery_carries_the_domain_out_of_both_sides() {
        let mut plan = correlated_set("UNION ALL", PLAIN);
        unnest::lower(&mut plan).expect("unnesting succeeds");
        plan.validate().expect("the rewritten plan is valid");
        let after = plan.to_string();
        assert!(!after.contains("DependentJoin"), "{after}");
        assert!(after.contains("SetOp UNION ALL"), "{after}");
        // Once per side. A set operation matches its sides by position, so a domain column coming
        // out of one side and not the other is two sides of different widths and an invalid plan.
        assert_eq!(after.matches("AS __branch_0, ").count(), 2, "{after}");
        // The side that asks nothing about the outer row is crossed with the domain, which is where
        // its copy of the column comes from.
        assert!(after.contains("CrossProduct"), "{after}");
    }

    #[test]
    fn an_except_inside_the_subquery_subtracts_inside_one_outer_row() {
        let mut plan = correlated_set("EXCEPT DISTINCT", ALSO);
        unnest::lower(&mut plan).expect("unnesting succeeds");
        plan.validate().expect("the rewritten plan is valid");
        let after = plan.to_string();
        assert!(!after.contains("DependentJoin"), "{after}");
        assert!(after.contains("SetOp EXCEPT DISTINCT"), "{after}");
        // Nothing is added to the operation itself. The domain column is part of the row, so a row
        // of one outer row and a row of another are two rows and neither subtracts the other.
        assert_eq!(after.matches("AS __branch_0, ").count(), 2, "{after}");
    }

    #[test]
    fn the_columns_a_side_had_stay_in_front_of_the_domain_columns() {
        let mut plan = correlated_set("INTERSECT DISTINCT", PLAIN);
        unnest::lower(&mut plan).expect("unnesting succeeds");
        plan.validate().expect("the rewritten plan is valid");
        let after = plan.to_string();
        assert!(!after.contains("DependentJoin"), "{after}");
        // One column each side had and the domain column behind it, in both sides, which is what
        // lets the operation above go back to matching by position.
        assert_eq!(after.matches("AS __branch_0, #").count(), 2, "{after}");
        assert!(after.contains("SetOp INTERSECT DISTINCT"), "{after}");
    }

    /// A join on the right of a dependent join, with the two sides written out.
    ///
    /// Which side reads the outer row is what these tests vary, so neither side comes from the
    /// shared fixture and both are given.
    fn correlated_join(kind: &str, left: &str, right: &str) -> Plan {
        let text = format!(
            "DependentJoin SINGLE on=[]\n  Get memory.main.outer AS o #0 [k::INTEGER]\n  Join {kind} on=[(#1.0::INTEGER = #5.0::INTEGER)::BOOLEAN]\n{left}{right}"
        );
        Plan::parse(&text).expect("a correlated join")
    }

    /// A side that reads the outer row, written at the depth a join's child sits at.
    const READS: &str = "    Filter (#1.0::INTEGER = #0.0::INTEGER)::BOOLEAN\n      Get memory.main.inner AS i #1 [k::INTEGER, value::INTEGER]\n";

    /// A side that asks nothing about the outer row.
    const QUIET: &str = "    Get memory.main.other AS u #5 [k::INTEGER, value::INTEGER]\n";

    #[test]
    fn a_right_join_puts_the_domain_on_the_side_it_preserves() {
        let mut plan = correlated_join("RIGHT", READS, QUIET);
        unnest::lower(&mut plan).expect("unnesting succeeds");
        plan.validate().expect("the rewritten plan is valid");
        let after = plan.to_string();
        assert!(!after.contains("DependentJoin"), "{after}");
        assert!(after.contains("Join RIGHT"), "{after}");
        // The quiet side asks nothing about the outer row and is crossed with the domain anyway,
        // because it is the side whose rows all survive and they have to say which outer row they
        // belong to. Two crosses is one per side.
        assert_eq!(after.matches("CrossProduct").count(), 2, "{after}");
        // One equality inside the join to line the two copies of the domain up, and one above it to
        // join back to the outer rows.
        assert_eq!(after.matches("IS NOT DISTINCT FROM").count(), 2, "{after}");
    }

    #[test]
    fn a_right_join_whose_correlated_side_is_the_one_it_preserves_needs_only_that_side() {
        let mut plan = correlated_join("RIGHT", QUIET, READS);
        unnest::lower(&mut plan).expect("unnesting succeeds");
        plan.validate().expect("the rewritten plan is valid");
        let after = plan.to_string();
        assert!(!after.contains("DependentJoin"), "{after}");
        assert!(after.contains("Join RIGHT"), "{after}");
        // The preserved side is the correlated one, so it carries the domain already and the other
        // side is left alone. One cross, and no second copy of the domain to line up with.
        assert_eq!(after.matches("CrossProduct").count(), 1, "{after}");
        assert_eq!(after.matches("IS NOT DISTINCT FROM").count(), 1, "{after}");
    }

    #[test]
    fn a_left_join_whose_correlated_side_is_the_right_one_carries_the_domain_on_both() {
        let mut plan = correlated_join("LEFT", QUIET, READS);
        unnest::lower(&mut plan).expect("unnesting succeeds");
        plan.validate().expect("the rewritten plan is valid");
        let after = plan.to_string();
        // This was refused before, and it is the mirror of the case above. The side the join
        // preserves is the quiet one, so that is the side that has to be given the domain.
        assert!(!after.contains("DependentJoin"), "{after}");
        assert!(after.contains("Join LEFT"), "{after}");
        assert_eq!(after.matches("CrossProduct").count(), 2, "{after}");
        assert_eq!(after.matches("IS NOT DISTINCT FROM").count(), 2, "{after}");
    }

    #[test]
    fn a_full_join_reads_whichever_copy_of_the_domain_is_filled_in() {
        let mut plan = correlated_join("FULL", READS, QUIET);
        unnest::lower(&mut plan).expect("unnesting succeeds");
        plan.validate().expect("the rewritten plan is valid");
        let after = plan.to_string();
        assert!(!after.contains("DependentJoin"), "{after}");
        assert!(after.contains("Join FULL"), "{after}");
        // Both sides are preserved, so both are given the domain and the two copies are equated.
        assert_eq!(after.matches("CrossProduct").count(), 2, "{after}");
        // The projection over the join is what the other kinds do not need. It picks the copy that
        // is there, which is a column neither side has.
        assert!(after.contains("CASE WHEN"), "{after}");
        assert!(after.contains("IS DISTINCT FROM NULL"), "{after}");
        assert!(after.contains("AS __domain_0"), "{after}");
        // The two copies have to be two columns. One domain node shared by both sides gives both of
        // them the same binding, and then the CASE reads one column twice and can never pick.
        assert_eq!(after.matches("groups=[#0.0::INTEGER] aggregates=[]").count(), 2, "{after}");
        let case = after.split("CASE WHEN ").nth(1).expect("a CASE in the plan");
        let then = case.split("THEN ").nth(1).expect("a THEN");
        let otherwise = case.split("ELSE ").nth(1).expect("an ELSE");
        let column = |text: &str| text.split_whitespace().next().expect("a column").to_owned();
        assert_ne!(column(then), column(otherwise), "{after}");
    }

    #[test]
    fn a_semi_join_carries_the_domain_on_the_side_whose_rows_come_out() {
        let mut plan = correlated_join("SEMI", READS, QUIET);
        unnest::lower(&mut plan).expect("unnesting succeeds");
        plan.validate().expect("the rewritten plan is valid");
        let after = plan.to_string();
        assert!(!after.contains("DependentJoin"), "{after}");
        assert!(after.contains("Join SEMI"), "{after}");
        // Only the left side. A semi join produces the left rows, the right side is consulted and
        // thrown away, and nothing in the right side asks about the outer row here.
        assert_eq!(after.matches("CrossProduct").count(), 1, "{after}");
        assert_eq!(after.matches("IS NOT DISTINCT FROM").count(), 1, "{after}");
    }

    #[test]
    fn a_semi_join_whose_correlated_side_is_the_right_one_carries_the_domain_on_both() {
        let mut plan = correlated_join("SEMI", QUIET, READS);
        unnest::lower(&mut plan).expect("unnesting succeeds");
        plan.validate().expect("the rewritten plan is valid");
        let after = plan.to_string();
        assert!(!after.contains("DependentJoin"), "{after}");
        assert!(after.contains("Join SEMI"), "{after}");
        // The left side carries one because its rows are the answer, and the right side carries one
        // because it is the side asking the question and it has to ask it about the same outer row.
        assert_eq!(after.matches("CrossProduct").count(), 2, "{after}");
        // Two copies of the domain, and they have to be two columns rather than one shared node, or
        // the equality between them says nothing and a left row meets every right row.
        assert_eq!(after.matches("groups=[#0.0::INTEGER] aggregates=[]").count(), 2, "{after}");
        // One equality inside the join to line the two copies up, and one above it to join back.
        assert_eq!(after.matches("IS NOT DISTINCT FROM").count(), 2, "{after}");
    }

    #[test]
    fn an_anti_join_is_the_semi_case_with_the_sense_flipped() {
        let mut plan = correlated_join("ANTI", QUIET, READS);
        unnest::lower(&mut plan).expect("unnesting succeeds");
        plan.validate().expect("the rewritten plan is valid");
        let after = plan.to_string();
        assert!(!after.contains("DependentJoin"), "{after}");
        assert!(after.contains("Join ANTI"), "{after}");
        assert_eq!(after.matches("CrossProduct").count(), 2, "{after}");
        assert_eq!(after.matches("IS NOT DISTINCT FROM").count(), 2, "{after}");
    }

    #[test]
    fn a_mark_join_decides_its_third_answer_per_domain_value() {
        let mut plan = correlated_join("MARK", QUIET, READS);
        unnest::lower(&mut plan).expect("unnesting succeeds");
        plan.validate().expect("the rewritten plan is valid");
        let after = plan.to_string();
        assert!(!after.contains("DependentJoin"), "{after}");
        assert!(after.contains("Join MARK"), "{after}");
        // The domain equality is inside the join rather than above it, which is the whole point.
        // A mark join answers true, false or unknown for each left row, and the row it answers for
        // is now a domain value and a left row together, so the right rows it is allowed to match
        // are the ones carrying the same domain value.
        assert_eq!(after.matches("CrossProduct").count(), 2, "{after}");
        assert_eq!(after.matches("IS NOT DISTINCT FROM").count(), 2, "{after}");
    }

    #[test]
    fn a_single_join_carries_the_domain_the_way_a_left_join_does() {
        let mut plan = correlated_join("SINGLE", READS, QUIET);
        unnest::lower(&mut plan).expect("unnesting succeeds");
        plan.validate().expect("the rewritten plan is valid");
        let after = plan.to_string();
        assert!(!after.contains("DependentJoin"), "{after}");
        assert!(after.contains("Join SINGLE"), "{after}");
        // Only the left side. A single join preserves its left rows and the right side asks nothing
        // about the outer row here, so there is one copy of the domain and one equality above it.
        assert_eq!(after.matches("CrossProduct").count(), 1, "{after}");
        assert_eq!(after.matches("IS NOT DISTINCT FROM").count(), 1, "{after}");
    }

    #[test]
    fn a_single_join_whose_right_side_reads_the_outer_row_carries_the_domain_on_both() {
        let mut plan = correlated_join("SINGLE", QUIET, READS);
        unnest::lower(&mut plan).expect("unnesting succeeds");
        plan.validate().expect("the rewritten plan is valid");
        let after = plan.to_string();
        assert!(!after.contains("DependentJoin"), "{after}");
        assert!(after.contains("Join SINGLE"), "{after}");
        // The left side carries one because its rows are preserved and a preserved row that matched
        // nothing is told nothing by the side it did not match. The right side carries one because
        // it is the side reading the outer row.
        assert_eq!(after.matches("CrossProduct").count(), 2, "{after}");
        assert_eq!(after.matches("groups=[#0.0::INTEGER] aggregates=[]").count(), 2, "{after}");
        assert_eq!(after.matches("IS NOT DISTINCT FROM").count(), 2, "{after}");
    }

    #[test]
    fn a_full_join_keeps_the_columns_it_did_not_replace() {
        let mut plan = correlated_join("FULL", QUIET, READS);
        unnest::lower(&mut plan).expect("unnesting succeeds");
        plan.validate().expect("the rewritten plan is valid");
        let after = plan.to_string();
        assert!(!after.contains("DependentJoin"), "{after}");
        // The projection rebinds everything under it, so every column the join produced has to be
        // listed again or the query above loses it. Four data columns across the two sides.
        assert!(after.contains("AS __kept_3"), "{after}");
        assert!(!after.contains("AS __kept_4"), "{after}");
    }
}
