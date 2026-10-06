//! When a grouped aggregate's key is a column the table is stored in ascending order of, so a group
//! is finished as soon as the key moves past it.
//!
//! A hash aggregate holds every group until the last row has arrived, because the next row could
//! belong to any of them. Over rows sorted on the key that is not true. Once the key has moved on,
//! the group behind it can never see another row, and a group that can never see another row does
//! not need a bucket, a hash, a probe, or a place in the merge between threads. TPC-H's lineitem is
//! stored in order key order, so `GROUP BY l_orderkey` in q18 is a million and a half groups that
//! each close within a few rows of opening.
//!
//! The executor does the work, a chunk at a time. What it needs from here is the promise that the
//! key arrives in ascending order, and that promise has two halves. The first is the store's: the
//! binder marks the columns whose summary says the rows never go down and hold no null, which is
//! `Plan::mark_ascending`. The second is the plan's: nothing between the scan and the aggregate may
//! reorder or merge rows. A filter drops rows and keeps the order of the rest, and a projection
//! that carries the column through unchanged keeps it too. Anything else, a join, a sort, a union,
//! stops the walk, because what comes out of it is in whatever order it chose.
//!
//! # What it does not do
//!
//! It does not trust the promise with the answer. The executor checks every chunk it is handed for
//! being in ascending order before it closes a group out of it, and a chunk that is not goes through
//! the hash table the way every chunk did before. A stale summary is a slower query and not a wrong
//! one.
//!
//! # Grouped
//!
//! Ascending is one proof that a group is finished when the key moves past it, and it is not the
//! only one. What the executor needs is that every value's rows are one run, and a table stored in
//! date order has `l_orderkey` that way without having it ascending: each order's lines are
//! together, and the orders come in date order. The file proves that with a forward link in the
//! monotone form that every child row followed to exactly one parent, because that form is only
//! taken when the children are in their parents' row order and the parent key is distinct. Such a
//! key is clustered as grouped, and the executor then closes the runs strictly inside a chunk
//! without asking the chunk to go up. That promise is not checked a chunk at a time, since no chunk
//! can see whether a value comes back later, and it does not need to be: the link is checked
//! against the table's generation when it is read, so it describes the rows being scanned.
//!
//! It is one key column and not several. A group by on two columns where the first is the sorted one
//! closes the same way in principle, and the executor side of that is a separate piece of work.
//!
//! # Through a join
//!
//! A probe answers its driving rows a chunk at a time and in their order, each row's matches next
//! to each other, so a key that arrives in runs at the probe leaves it in the same runs. The walk
//! goes through an inner join to the side that drives it, and through a semi or anti join that is
//! not turned around, when the condition holds an equality the probe can key on. Without one the
//! executor builds the general join, which collects the driving side first, and the walk stops.
//!
//! # Keys the run key fixes
//!
//! TPC-H q03 groups on `l_orderkey, o_orderdate, o_shippriority`. The rows are lineitems joined to
//! their order on `l_orderkey = o_orderkey`, and `o_orderkey` holds no value twice in `orders`, so
//! one `l_orderkey` is one order and one `o_orderdate`. The last two keys never split a group the
//! first one makes, but with three keys the aggregate cannot close anything and hashes every row.
//! So when one key arrives in runs and fixes every other key, the others leave the grouping and
//! come back as `min` calls, which over a group whose values are all the same is that value, a
//! null included. A projection above puts the columns back in the order the aggregate had them.
//!
//! What fixes what is read off the rows under the aggregate. An equality between two columns that
//! every row passed, in a filter or an inner join, makes them one value. A column of a scan that
//! holds every value once, by exact counts or by a link's uniqueness certificate, fixes every column
//! of that scan, and so do all the keys of an aggregate below fix its calls. Equalities count only
//! over types where equal values are the same value, which leaves out floating point, where `0.0`
//! and `-0.0` are equal and different, and strings, which a collation may compare loosely.

use rudb_common::rules::Rule;
use rudb_common::{LogicalType, Result};
use rudb_plan::{
    BuildSide, ColumnBinding, CompareOp, ConjunctionOp, Expr, ExprRef, JoinKind, Node, NodeRef,
    Plan, Slice,
};

use crate::estimate::Facts;
use crate::link::Linked;
use crate::pass::{Context, Pass, top_down};
use crate::{unique, walk};

/// Marks every grouped aggregate whose one key arrives in ascending order.
///
/// A rudb name rather than a DuckDB one, for the reason [`crate::dense::AggregateDense`] gives.
/// `SET disabled_optimizers = 'aggregate_cluster'` turns the pass off, and `SET stats_closed_groups
/// = 'off'` turns off [`Rule::ClosedGroups`], which is the rule.
#[derive(Debug)]
pub struct AggregateCluster;

impl Pass for AggregateCluster {
    fn name(&self) -> &'static str {
        "aggregate_cluster"
    }

    fn run(&self, plan: &mut Plan, context: &Context) -> Result<()> {
        if context.allows(Rule::ClosedGroups) {
            cluster(plan, context.links(), context.facts());
        }
        Ok(())
    }
}

/// Records every aggregate in `plan` whose key the walk down to its scan keeps in order, after
/// narrowing the ones whose other keys that key fixes.
fn cluster(plan: &mut Plan, links: &[Linked], stats: &Facts) {
    let mut changed = false;
    let root = walk::restack(plan, plan.root(), &mut changed, &mut |plan, at| {
        narrow(plan, at, links, stats)
    });
    if changed {
        plan.set_root(root);
    }
    let mut found = Vec::new();
    for node in top_down(plan) {
        let Node::Aggregate { input, index, groups, .. } = *plan.node(node) else {
            continue;
        };
        let &[key] = plan.expr_list(groups) else { continue };
        let &Expr::Column(binding) = plan.expr(key) else { continue };
        match sorted(plan, input, binding, links, 16) {
            Some(Run::Ascending) => found.push((index, false)),
            Some(Run::Grouped) => found.push((index, true)),
            None => {}
        }
    }
    for (index, grouped) in found {
        if grouped {
            plan.cluster_grouped(index);
        } else {
            plan.cluster(index);
        }
    }
}

/// How a key arrives at an aggregate that can close its groups.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Run {
    /// Never going down, which each chunk is also checked for.
    Ascending,
    /// Each value one run of rows, the runs in no order.
    Grouped,
}

/// Whether `binding`, read off the output of `node`, is a stored column in ascending order, or one
/// the file proves grouped.
///
/// The budget is against a malformed plan, the way it is in the estimator's walk.
fn sorted(
    plan: &Plan,
    node: NodeRef,
    binding: ColumnBinding,
    links: &[Linked],
    depth: u32,
) -> Option<Run> {
    let depth = depth.checked_sub(1)?;
    match *plan.node(node) {
        Node::Filter { input, .. } => sorted(plan, input, binding, links, depth),
        Node::Project { input, index, exprs, .. } if index == binding.table => {
            let &carried = plan.expr_list(exprs).get(binding.column as usize)?;
            let &Expr::Column(carried) = plan.expr(carried) else { return None };
            sorted(plan, input, carried, links, depth)
        }
        Node::Get { index, table, columns, .. } if index == binding.table => {
            let field = plan.field_list(columns).get(binding.column as usize)?;
            if plan.ascending(index, &field.name) {
                return Some(Run::Ascending);
            }
            let table = plan.string(table);
            links
                .iter()
                .any(|link| {
                    link.groups_child()
                        && link.child.eq_ignore_ascii_case(table)
                        && link.child_column.eq_ignore_ascii_case(&field.name)
                })
                .then_some(Run::Grouped)
        }
        Node::Join { left, right, kind, conditions, build } => {
            let driving = match (kind, build) {
                (JoinKind::Inner, BuildSide::Left) => right,
                (JoinKind::Inner | JoinKind::Semi | JoinKind::Anti, BuildSide::Right) => left,
                _ => return None,
            };
            let held = if driving == left { right } else { left };
            if !produces(plan, driving, binding) || !probed(plan, conditions, driving, held) {
                return None;
            }
            sorted(plan, driving, binding, links, depth)
        }
        _ => None,
    }
}

/// Whether `binding` is one of the columns `at` hands up.
fn produces(plan: &Plan, at: NodeRef, binding: ColumnBinding) -> bool {
    walk::outputs(plan, at)
        .is_some_and(|columns| columns.iter().any(|(bound, _)| *bound == binding))
}

/// Whether the join's condition holds an equality the executor's probe keys on, which is what
/// decides between the probe and the general join that collects its driving side first.
///
/// The probe takes any expression over one side against one over the other, of one keyed type. This
/// asks for two plain columns, which is the join every measured query has and is narrower than the
/// probe, so a join this says yes to is always a probe.
fn probed(plan: &Plan, conditions: Slice, driving: NodeRef, held: NodeRef) -> bool {
    plan.expr_list(conditions).iter().any(|&condition| {
        let Expr::Compare { op: CompareOp::Equal | CompareOp::NotDistinctFrom, left, right } =
            *plan.expr(condition)
        else {
            return false;
        };
        let (&Expr::Column(one), &Expr::Column(other)) = (plan.expr(left), plan.expr(right)) else {
            return false;
        };
        plan.expr_type(left) == plan.expr_type(right)
            && plan.expr_type(left).is_keyed()
            && ((produces(plan, driving, one) && produces(plan, held, other))
                || (produces(plan, driving, other) && produces(plan, held, one)))
    })
}

/// The aggregate `at` grouped on one of its keys alone, when that key arrives in runs and fixes the
/// rest, which come back as `min` calls under a projection that keeps the aggregate's index and
/// column order.
///
/// An aggregate the dense pass marked keeps its keys, since an array indexed by the key is already
/// cheaper than closing groups, and so does one with a `DISTINCT` call, which the executor does not
/// close.
fn narrow(plan: &mut Plan, at: NodeRef, links: &[Linked], stats: &Facts) -> Option<NodeRef> {
    let Node::Aggregate { input, index, groups, aggregates } = *plan.node(at) else { return None };
    let keys = plan.expr_list(groups).to_vec();
    if keys.len() < 2 || plan.dense(index).is_some() {
        return None;
    }
    let calls = plan.expr_list(aggregates).to_vec();
    if calls.iter().any(|&call| matches!(plan.expr(call), Expr::Aggregate { distinct: true, .. })) {
        return None;
    }
    let bindings = keys
        .iter()
        .map(|&key| match *plan.expr(key) {
            Expr::Column(binding) => Some(binding),
            _ => None,
        })
        .collect::<Option<Vec<_>>>()?;
    if keys.iter().any(|&key| plan.expr_type(key).is_nested() || !plan.expr_type(key).is_keyed()) {
        return None;
    }
    let mut known = Fixed::default();
    known.gather(plan, input, links, stats, 32);
    let lead = (0..keys.len()).find(|&lead| {
        exact(plan.expr_type(keys[lead]))
            && sorted(plan, input, bindings[lead], links, 16).is_some()
            && bindings.iter().all(|&other| known.fixes(bindings[lead], other))
    })?;

    let staged = walk::fresh_index(plan);
    let name = plan.intern("min");
    let mut staged_calls = calls.clone();
    for (position, &key) in keys.iter().enumerate() {
        if position != lead {
            let args = plan.add_expr_list(&[key]);
            let call = Expr::Aggregate { name, args, distinct: false, filter: None };
            let (ty, span) = (plan.expr_type(key).clone(), plan.expr_span(key));
            staged_calls.push(plan.add_expr_at(call, ty, span));
        }
    }
    let kept = plan.add_expr_list(&[keys[lead]]);
    let staged_calls = plan.add_expr_list(&staged_calls);
    let inner = plan.add_node(Node::Aggregate {
        input,
        index: staged,
        groups: kept,
        aggregates: staged_calls,
    });
    if let Some(groups) = plan.presized(index) {
        plan.presize(staged, groups);
    }

    let mut projected = Vec::with_capacity(keys.len() + calls.len());
    let mut extra = 1 + calls.len();
    for (position, &key) in keys.iter().enumerate() {
        if position == lead {
            projected.push(column(plan, staged, 0, key));
        } else {
            projected.push(column(plan, staged, extra, key));
            extra += 1;
        }
    }
    for (offset, &call) in calls.iter().enumerate() {
        projected.push(column(plan, staged, 1 + offset, call));
    }
    let names: Vec<_> =
        (0..projected.len()).map(|position| plan.intern(&format!("column{position}"))).collect();
    let exprs = plan.add_expr_list(&projected);
    let names = plan.add_name_list(&names);
    Some(plan.add_node(Node::Project { input: inner, index, exprs, names }))
}

/// A reference to column `position` of `table`, typed and placed like `source`.
fn column(plan: &mut Plan, table: u32, position: usize, source: ExprRef) -> ExprRef {
    let position = u32::try_from(position).expect("an aggregate cannot have this many columns");
    let ty = plan.expr_type(source).clone();
    let span = plan.expr_span(source);
    plan.add_expr_at(Expr::Column(ColumnBinding::new(table, position)), ty, span)
}

/// What the rows under an aggregate say about which columns fix which.
#[derive(Debug, Default)]
struct Fixed {
    /// Pairs of columns every row holds the same value in.
    equal: Vec<(ColumnBinding, ColumnBinding)>,
    /// Sets of columns that, all known, fix every column of the table index they belong to.
    keys: Vec<Vec<ColumnBinding>>,
}

impl Fixed {
    /// Reads the equalities and the keys off the operators under `at` that pass rows on unchanged.
    ///
    /// Through a filter and both sides of an inner join or a cross product, whose conditions every
    /// row that comes out has passed, through the left side of a semi, an anti or a left join,
    /// whose rows come out as they went in, and through a projection, whose plain column references
    /// are renames. An aggregate's keys fix its calls, and the walk stops there, since nothing said
    /// about the rows under it is true of the groups. The budget is against a malformed plan.
    fn gather(&mut self, plan: &Plan, at: NodeRef, links: &[Linked], stats: &Facts, depth: u32) {
        let Some(depth) = depth.checked_sub(1) else { return };
        match *plan.node(at) {
            Node::Filter { input, predicate } => {
                self.equalities(plan, predicate);
                self.gather(plan, input, links, stats, depth);
            }
            Node::Project { input, index, exprs, .. } => {
                for (position, &expr) in plan.expr_list(exprs).iter().enumerate() {
                    if let Expr::Column(inner) = *plan.expr(expr) {
                        let position = u32::try_from(position).unwrap_or(u32::MAX);
                        self.equal.push((ColumnBinding::new(index, position), inner));
                    }
                }
                self.gather(plan, input, links, stats, depth);
            }
            Node::Join { left, right, kind, conditions, .. } => match kind {
                JoinKind::Inner => {
                    for &condition in plan.expr_list(conditions) {
                        self.equalities(plan, condition);
                    }
                    self.gather(plan, left, links, stats, depth);
                    self.gather(plan, right, links, stats, depth);
                }
                JoinKind::Semi | JoinKind::Anti | JoinKind::Left => {
                    self.gather(plan, left, links, stats, depth);
                }
                _ => {}
            },
            Node::CrossProduct { left, right } => {
                self.gather(plan, left, links, stats, depth);
                self.gather(plan, right, links, stats, depth);
            }
            Node::Get { index, table, columns, .. } => {
                let table = plan.string(table);
                for (position, field) in plan.field_list(columns).iter().enumerate() {
                    let binding = ColumnBinding::new(index, u32::try_from(position).unwrap_or(0));
                    let linked = links.iter().any(|link| {
                        link.unique
                            && link.second.is_none()
                            && link.parent.eq_ignore_ascii_case(table)
                            && link.parent_column.eq_ignore_ascii_case(&field.name)
                    });
                    if linked || unique::counted(plan, at, binding, stats) {
                        self.keys.push(vec![binding]);
                    }
                }
            }
            Node::Aggregate { index, groups, .. } => {
                let count = u32::try_from(plan.expr_list(groups).len()).unwrap_or(u32::MAX);
                if count > 0 {
                    self.keys.push((0..count).map(|key| ColumnBinding::new(index, key)).collect());
                }
            }
            _ => {}
        }
    }

    /// Records the column equalities in the `AND` of `predicate`.
    fn equalities(&mut self, plan: &Plan, predicate: ExprRef) {
        match *plan.expr(predicate) {
            Expr::Conjunction { op: ConjunctionOp::And, children } => {
                for &child in plan.expr_list(children) {
                    self.equalities(plan, child);
                }
            }
            Expr::Compare { op: CompareOp::Equal, left, right } => {
                let (&Expr::Column(one), &Expr::Column(other)) =
                    (plan.expr(left), plan.expr(right))
                else {
                    return;
                };
                if plan.expr_type(left) == plan.expr_type(right) && exact(plan.expr_type(left)) {
                    self.equal.push((one, other));
                }
            }
            _ => {}
        }
    }

    /// Whether every row with the same value of `from` holds the same value of `to`.
    fn fixes(&self, from: ColumnBinding, to: ColumnBinding) -> bool {
        let mut columns = vec![from];
        let mut tables: Vec<u32> = Vec::new();
        let known = |columns: &[ColumnBinding], tables: &[u32], binding: &ColumnBinding| {
            columns.contains(binding) || tables.contains(&binding.table)
        };
        loop {
            let mut grew = false;
            for &(one, other) in &self.equal {
                let (a, b) = (known(&columns, &tables, &one), known(&columns, &tables, &other));
                if a != b {
                    columns.push(if a { other } else { one });
                    grew = true;
                }
            }
            for key in &self.keys {
                let table = key[0].table;
                if !tables.contains(&table)
                    && key.iter().all(|binding| known(&columns, &tables, binding))
                {
                    tables.push(table);
                    grew = true;
                }
            }
            if known(&columns, &tables, &to) {
                return true;
            }
            if !grew {
                return false;
            }
        }
    }
}

/// Whether two equal values of `ty` are the same value, so that a column equal to a key fixes
/// whatever the key fixes.
fn exact(ty: &LogicalType) -> bool {
    ty.is_integer() || matches!(ty, LogicalType::Date | LogicalType::Decimal { .. })
}

#[cfg(test)]
mod tests {
    use rudb_plan::Plan;

    use super::{AggregateCluster, Rule};
    use crate::pass::{Context, Pass};

    const SCAN: &str = "Get memory.main.t AS t #0 [a::INTEGER, b::INTEGER]";

    fn plan(text: &str) -> Plan {
        let mut plan = Plan::parse(text).expect("a plan that parses");
        plan.mark_ascending(0, "a");
        plan
    }

    fn run(plan: &mut Plan, context: &Context) {
        AggregateCluster.run(plan, context).expect("a pass that cannot fail");
    }

    #[test]
    fn a_key_the_table_is_sorted_on_is_clustered() {
        let mut plan =
            plan(&format!("Aggregate #1 groups=[#0.0::INTEGER] aggregates=[]\n  {SCAN}\n"));
        run(&mut plan, &Context::new());
        assert!(plan.clustered(1));
    }

    #[test]
    fn a_key_the_table_is_not_sorted_on_is_left_alone() {
        let mut plan =
            plan(&format!("Aggregate #1 groups=[#0.1::INTEGER] aggregates=[]\n  {SCAN}\n"));
        run(&mut plan, &Context::new());
        assert!(!plan.clustered(1));
    }

    #[test]
    fn two_keys_are_left_alone() {
        let mut plan = plan(&format!(
            "Aggregate #1 groups=[#0.0::INTEGER, #0.1::INTEGER] aggregates=[]\n  {SCAN}\n"
        ));
        run(&mut plan, &Context::new());
        assert!(!plan.clustered(1));
    }

    #[test]
    fn a_filter_keeps_the_order() {
        let mut plan = plan(&format!(
            "Aggregate #1 groups=[#0.0::INTEGER] aggregates=[]\n  Filter (#0.1::INTEGER > 3::INTEGER)::BOOLEAN\n    {SCAN}\n"
        ));
        run(&mut plan, &Context::new());
        assert!(plan.clustered(1));
    }

    fn linked(link: crate::link::Linked) -> Context {
        let mut context = Context::new();
        context.relate(std::sync::Arc::new(vec![link]));
        context
    }

    #[test]
    fn a_key_a_monotone_total_link_proves_grouped_is_clustered_as_grouped() {
        // Column `b` is not ascending. A link from it that every row followed, stored monotone, is
        // what a date ordered `lineitem` has on `l_orderkey`.
        let text = format!("Aggregate #1 groups=[#0.1::INTEGER] aggregates=[]\n  {SCAN}\n");
        let proof = crate::link::Linked::verified("t", "b", "p", "k").monotone();
        let mut marked = plan(&text);
        run(&mut marked, &linked(proof.clone()));
        assert!(marked.clustered(1));
        assert!(marked.grouped(1), "grouped, so the operator does not expect it to go up");

        // An ascending key stays ascending, and keeps its per chunk check.
        let mut ascending =
            plan(&format!("Aggregate #1 groups=[#0.0::INTEGER] aggregates=[]\n  {SCAN}\n"));
        run(&mut ascending, &linked(proof.clone()));
        assert!(ascending.clustered(1) && !ascending.grouped(1));

        // Each missing certificate leaves it alone: a packed link, a partial one, one over two
        // columns, and one from another column.
        for weaker in [
            crate::link::Linked::verified("t", "b", "p", "k"),
            crate::link::Linked::built("t", "b", "p", "k").monotone(),
            proof.clone().and("a", "j"),
            crate::link::Linked::verified("t", "a", "p", "k").monotone(),
        ] {
            let mut marked = plan(&text);
            run(&mut marked, &linked(weaker.clone()));
            assert!(!marked.clustered(1), "{weaker:?}");
        }
    }

    /// TPC-H q03's aggregate: lineitem drives a probe into orders and the aggregate groups on the
    /// lineitem key and two columns of the order it found.
    fn joined(build: &str, groups: &str) -> String {
        format!(
            "Aggregate #3 groups=[{groups}] aggregates=[sum(#2.1::BIGINT)::HUGEINT]\n  Join INNER on=[(#2.0::BIGINT = #1.0::BIGINT)::BOOLEAN] build={build}\n    Get memory.main.orders AS orders #1 [o_orderkey::BIGINT, o_custkey::BIGINT, o_orderdate::DATE, o_shippriority::INTEGER]\n    Filter (#2.1::BIGINT > 3::BIGINT)::BOOLEAN\n      Get memory.main.lineitem AS lineitem #2 [l_orderkey::BIGINT, l_price::BIGINT]\n"
        )
    }

    const Q03: &str = "#2.0::BIGINT, #1.2::DATE, #1.3::INTEGER";

    fn narrowed(text: &str, links: Vec<crate::link::Linked>) -> Plan {
        let mut plan = Plan::parse(text).expect("a plan that parses");
        plan.mark_ascending(2, "l_orderkey");
        let mut context = Context::new();
        context.relate(std::sync::Arc::new(links));
        run(&mut plan, &context);
        plan.validate().unwrap_or_else(|error| panic!("{plan} did not stay valid: {error}"));
        plan
    }

    fn orders() -> Vec<crate::link::Linked> {
        vec![crate::link::Linked::built("lineitem", "l_orderkey", "orders", "o_orderkey")]
    }

    #[test]
    fn keys_the_run_key_fixes_through_a_unique_parent_become_least_values() {
        let plan = narrowed(&joined("left", Q03), orders());
        let text = plan.to_string();
        assert!(
            text.starts_with("Project #3 [#4.0::BIGINT AS column0, #4.2::DATE AS column1, #4.3::INTEGER AS column2, #4.1::HUGEINT AS column3]"),
            "{text}"
        );
        assert!(
            text.contains("Aggregate #4 groups=[#2.0::BIGINT] aggregates=[sum(#2.1::BIGINT)::HUGEINT, min(#1.2::DATE)::DATE, min(#1.3::INTEGER)::INTEGER]"),
            "{text}"
        );
        assert!(plan.clustered(4) && !plan.clustered(3));
    }

    #[test]
    fn the_order_key_of_the_side_that_is_built_does_not_arrive_in_runs() {
        // Built on the right, so orders drives and lineitem is the table the probe looks in.
        let before = joined("right", Q03);
        let plan = narrowed(&before, orders());
        assert_eq!(plan.to_string(), Plan::parse(&before).expect("parses").to_string());
        assert!(!plan.clustered(3));
    }

    #[test]
    fn keys_nothing_proves_fixed_stay_keys() {
        // No link, so nothing says orders holds each key once.
        let before = joined("left", Q03);
        let plan = narrowed(&before, Vec::new());
        assert!(!plan.to_string().contains("min("), "{plan}");
        assert!(!plan.clustered(3));

        // An order has many lines, so the order key fixes no column of lineitem but its own.
        let before = joined("left", "#2.0::BIGINT, #2.1::BIGINT");
        let plan = narrowed(&before, orders());
        assert!(!plan.to_string().contains("min("), "{plan}");
    }

    #[test]
    fn a_driving_key_with_one_key_clusters_through_the_probe() {
        let plan = narrowed(&joined("left", "#2.0::BIGINT"), orders());
        assert!(plan.clustered(3));
        let plan = narrowed(&joined("right", "#2.0::BIGINT"), orders());
        assert!(!plan.clustered(3));
    }

    #[test]
    fn the_rule_s_own_setting_turns_it_off() {
        let mut plan =
            plan(&format!("Aggregate #1 groups=[#0.0::INTEGER] aggregates=[]\n  {SCAN}\n"));
        let mut context = Context::new();
        let mut rules = rudb_common::rules::Rules::default();
        rules.set(Rule::ClosedGroups, false);
        context.govern(rules);
        run(&mut plan, &context);
        assert!(!plan.clustered(1));
    }
}
