//! Reading a total off the groups that already add it up, rather than running its query twice.
//!
//! TPC-H q11 groups the German part supplies by part and keeps the parts worth more than a small
//! share of the total, and the total is a subquery over the same three tables with the same filter.
//! Written as it is, the join of partsupp, supplier and nation runs twice, once for the groups and
//! once for the total, and the second run is close to half of what the query costs.
//!
//! A sum of the group sums is the sum, and the same holds for the smallest of the group minimums and
//! the largest of the maximums. So when an aggregate with no groups reads rows that are provably the
//! rows a grouped aggregate reads, and each of its calls is one of the grouped calls, the grouped
//! aggregate is run once and held, and the total is added up over the held groups. For q11 that is
//! some thirty thousand groups read again in place of a join over eight hundred thousand rows.
//!
//! # When it is the same answer
//!
//! The two inputs have to be the same query, which is checked the dull way: the same operators in
//! the same shape over the same tables, with the same predicates and expressions once each column of
//! the total's side is mapped to the column of the grouped side it stands for. A scan of the total's
//! side may read fewer columns than the grouped one, since column pruning has usually been at it, and
//! a projection or an inner aggregate may produce fewer of its outputs, but never a different one.
//! Only filters, projections, cross products, inner, left, semi and anti joins, aggregates and scans
//! are compared, so nothing that reads a column from outside the subtree or that can answer twice
//! differently is ever called the same.
//!
//! The calls have to be plain `sum`, `min` or `max` with no `DISTINCT` and no `FILTER`, and a `sum`
//! has to be over a decimal. An exact decimal total of totals is the total, where a floating point
//! one is a different rounding. Nulls come out the same: a group whose values are all null sums to
//! null and a sum skips it, and no groups at all is a sum over nothing, which is null, which is also
//! what the total over no rows is.
//!
//! # What it produces
//!
//! ```text
//! Aggregate #3 groups=[k] aggregates=[sum(e)]      over X
//! Aggregate #7 groups=[] aggregates=[sum(e')]      over X'
//! ```
//!
//! becomes a materialisation of the first at the root, a read of it where the first was, and in
//! place of the second an aggregate with no groups over a second read of it:
//!
//! ```text
//! MaterializedCte groups @c
//!   Aggregate #10 groups=[k] aggregates=[sum(e)]   over X
//!   ...
//!     CteScan groups @c #3
//!     ...
//!       Aggregate #7 groups=[] aggregates=[sum(#11.1)]
//!         CteScan groups @c #11
//! ```
//!
//! Everything above either aggregate reads the index it read before. A second run finds a total over
//! a read of held rows, which is not a query any grouped aggregate reads, and stops.
//!
//! See `spec/perf/48-total-from-groups.md`.

use std::collections::HashMap;

use rudb_common::{Field, LogicalType, Result};
use rudb_plan::{ColumnBinding, Expr, ExprRef, JoinKind, Node, NodeRef, Plan, Slice};

use crate::pass::{Context, Pass};
use crate::walk;

/// Answers a total from the groups of the same query.
#[derive(Debug, Clone, Copy)]
pub struct TotalFromGroups;

impl Pass for TotalFromGroups {
    fn name(&self) -> &'static str {
        "total_from_groups"
    }

    fn run(&self, plan: &mut Plan, _context: &Context) -> Result<()> {
        while let Some(found) = pair(plan) {
            rewrite(plan, &found);
        }
        Ok(())
    }
}

/// A grouped aggregate and a total over the same rows, with where each total call is among the
/// grouped calls.
struct Found {
    grouped: NodeRef,
    total: NodeRef,
    calls: Vec<usize>,
}

/// The first grouped aggregate and total in `plan` that this applies to.
fn pair(plan: &Plan) -> Option<Found> {
    let mut aggregates = Vec::new();
    reachable(plan, plan.root(), false, &mut aggregates);
    for &total in &aggregates {
        let Node::Aggregate { input: below, groups, aggregates: calls, .. } = *plan.node(total)
        else {
            continue;
        };
        if !groups.is_empty() {
            continue;
        }
        for &grouped in &aggregates {
            let Node::Aggregate { input, groups, aggregates: held, .. } = *plan.node(grouped)
            else {
                continue;
            };
            if groups.is_empty() {
                continue;
            }
            let mut map = HashMap::new();
            if !mirror(plan, input, below, &mut map) {
                continue;
            }
            let lookup = |binding| map.get(&binding).copied();
            let held = plan.expr_list(held);
            let found: Option<Vec<usize>> = plan
                .expr_list(calls)
                .iter()
                .map(|&call| {
                    if !additive(plan, call) {
                        return None;
                    }
                    held.iter().position(|&one| walk::same_mapped(plan, one, call, &lookup))
                })
                .collect();
            if let Some(calls) = found {
                return Some(Found { grouped, total, calls });
            }
        }
    }
    None
}

/// Every aggregate under `at`, leaving out the definitions of materialisations, which are already
/// run once and held.
fn reachable(plan: &Plan, at: NodeRef, held: bool, found: &mut Vec<NodeRef>) {
    let node = plan.node(at);
    if matches!(node, Node::Aggregate { .. }) && !held && !found.contains(&at) {
        found.push(at);
    }
    if let Node::MaterializedCte { definition, body, .. } = *node {
        reachable(plan, definition, true, found);
        reachable(plan, body, false, found);
        return;
    }
    for child in node.children().into_iter().flatten() {
        reachable(plan, child, false, found);
    }
}

/// Whether a total of this call over groups of it is the call over the rows.
fn additive(plan: &Plan, call: ExprRef) -> bool {
    let Expr::Aggregate { name, args, distinct, filter } = *plan.expr(call) else { return false };
    if distinct || filter.is_some() || plan.expr_list(args).len() != 1 {
        return false;
    }
    match plan.string(name) {
        "min" | "max" => true,
        "sum" => matches!(plan.expr_type(call), LogicalType::Decimal { .. }),
        _ => false,
    }
}

/// Whether `other` produces the rows `one` does, filling `map` with the column of `one` each column
/// of `other` stands for.
fn mirror(
    plan: &Plan,
    one: NodeRef,
    other: NodeRef,
    map: &mut HashMap<ColumnBinding, ColumnBinding>,
) -> bool {
    match (plan.node(one), plan.node(other)) {
        (
            &Node::Get { catalog, schema, table, index, columns, .. },
            &Node::Get {
                catalog: other_catalog,
                schema: other_schema,
                table: other_table,
                index: other_index,
                columns: other_columns,
                ..
            },
        ) => {
            if plan.string(catalog) != plan.string(other_catalog)
                || plan.string(schema) != plan.string(other_schema)
                || plan.string(table) != plan.string(other_table)
            {
                return false;
            }
            let fields = plan.field_list(columns);
            for (at, field) in plan.field_list(other_columns).iter().enumerate() {
                let Some(to) =
                    fields.iter().position(|one| one.name == field.name && one.ty == field.ty)
                else {
                    return false;
                };
                map.insert(binding(other_index, at), binding(index, to));
            }
            true
        }
        (
            &Node::Filter { input, predicate },
            &Node::Filter { input: other_input, predicate: other_predicate },
        ) => mirror(plan, input, other_input, map) && same(plan, predicate, other_predicate, map),
        (
            &Node::Project { input, index, exprs, .. },
            &Node::Project { input: other_input, index: other_index, exprs: other_exprs, .. },
        ) => {
            mirror(plan, input, other_input, map)
                && produced(plan, (index, 0, exprs), (other_index, 0, other_exprs), map)
        }
        (
            &Node::CrossProduct { left, right },
            &Node::CrossProduct { left: other_left, right: other_right },
        ) => mirror(plan, left, other_left, map) && mirror(plan, right, other_right, map),
        (
            &Node::Join { left, right, kind, conditions, .. },
            &Node::Join {
                left: other_left,
                right: other_right,
                kind: other_kind,
                conditions: other_conditions,
                ..
            },
        ) => {
            let joined =
                matches!(kind, JoinKind::Inner | JoinKind::Left | JoinKind::Semi | JoinKind::Anti);
            let (conditions, other_conditions) =
                (plan.expr_list(conditions), plan.expr_list(other_conditions));
            joined
                && kind == other_kind
                && conditions.len() == other_conditions.len()
                && mirror(plan, left, other_left, map)
                && mirror(plan, right, other_right, map)
                && conditions
                    .iter()
                    .zip(other_conditions)
                    .all(|(&one, &other)| same(plan, one, other, map))
        }
        (
            &Node::Aggregate { input, index, groups, aggregates },
            &Node::Aggregate {
                input: other_input,
                index: other_index,
                groups: other_groups,
                aggregates: other_aggregates,
            },
        ) => {
            let (keys, other_keys) = (plan.expr_list(groups), plan.expr_list(other_groups));
            if keys.len() != other_keys.len() || !mirror(plan, input, other_input, map) {
                return false;
            }
            for (at, (&key, &other)) in keys.iter().zip(other_keys).enumerate() {
                if !same(plan, key, other, map) {
                    return false;
                }
                map.insert(binding(other_index, at), binding(index, at));
            }
            produced(
                plan,
                (index, keys.len(), aggregates),
                (other_index, keys.len(), other_aggregates),
                map,
            )
        }
        _ => false,
    }
}

/// Whether each expression `other` produces is one `one` produces, recording where, where each
/// side is its index, the position its expressions start at and the expressions.
fn produced(
    plan: &Plan,
    one: (u32, usize, Slice),
    other: (u32, usize, Slice),
    map: &mut HashMap<ColumnBinding, ColumnBinding>,
) -> bool {
    let exprs = plan.expr_list(one.2);
    for (at, &expr) in plan.expr_list(other.2).iter().enumerate() {
        let Some(to) = exprs.iter().position(|&held| same(plan, held, expr, map)) else {
            return false;
        };
        map.insert(binding(other.0, other.1 + at), binding(one.0, one.1 + to));
    }
    true
}

/// Whether `other` is `one` under `map`, and neither can answer differently when asked again.
fn same(
    plan: &Plan,
    one: ExprRef,
    other: ExprRef,
    map: &HashMap<ColumnBinding, ColumnBinding>,
) -> bool {
    !walk::volatile(plan, other)
        && walk::same_mapped(plan, one, other, &|binding| map.get(&binding).copied())
}

fn binding(index: u32, position: usize) -> ColumnBinding {
    ColumnBinding::new(index, u32::try_from(position).expect("a column count fits in a u32"))
}

/// Holds the grouped aggregate, reads it where it was, and adds the total up over a second read.
fn rewrite(plan: &mut Plan, found: &Found) {
    let Node::Aggregate { input, index, groups, aggregates } = *plan.node(found.grouped) else {
        return;
    };
    let Node::Aggregate { index: total_index, aggregates: total_calls, .. } =
        *plan.node(found.total)
    else {
        return;
    };
    let keys = plan.expr_list(groups).len();
    let fields: Vec<Field> = plan
        .expr_list(groups)
        .iter()
        .chain(plan.expr_list(aggregates))
        .enumerate()
        .map(|(at, &expr)| Field::new(format!("column{at}"), plan.expr_type(expr).clone()))
        .collect();
    let held = walk::fresh_index(plan);
    let read = held + 1;
    let cte = next_cte(plan);
    let name = plan.intern("groups");
    let columns = plan.add_fields(&fields);

    let calls = plan.expr_list(total_calls).to_vec();
    let mut built = Vec::with_capacity(calls.len());
    for (&call, &at) in calls.iter().zip(&found.calls) {
        let Expr::Aggregate { name: called, .. } = *plan.expr(call) else { return };
        let position = u32::try_from(keys + at).expect("an aggregate this wide cannot bind");
        let ty = fields[keys + at].ty.clone();
        let column = plan.add_expr(Expr::Column(ColumnBinding::new(read, position)), ty);
        let args = plan.add_expr_list(&[column]);
        let span = plan.expr_span(call);
        let ty = plan.expr_type(call).clone();
        built.push(plan.add_expr_at(
            Expr::Aggregate { name: called, args, distinct: false, filter: None },
            ty,
            span,
        ));
    }
    let built = plan.add_expr_list(&built);
    let empty = plan.add_expr_list(&[]);
    let scan = plan.add_node(Node::CteScan { index: read, cte, name, columns });
    let total = plan.add_node(Node::Aggregate {
        input: scan,
        index: total_index,
        groups: empty,
        aggregates: built,
    });
    let reread = plan.add_node(Node::CteScan { index, cte, name, columns });

    let (grouped, replaced) = (found.grouped, found.total);
    let mut changed = false;
    let body = walk::restack(plan, plan.root(), &mut changed, &mut |_, at| {
        if at == grouped {
            Some(reread)
        } else if at == replaced {
            Some(total)
        } else {
            None
        }
    });
    let definition = plan.add_node(Node::Aggregate { input, index: held, groups, aggregates });
    let root = plan.add_node(Node::MaterializedCte { definition, body, name, cte, columns });
    plan.set_root(root);
}

/// A materialisation number nothing in the plan is using.
fn next_cte(plan: &Plan) -> u32 {
    let mut next = 0;
    for at in 0..plan.node_count() {
        if let Node::MaterializedCte { cte, .. } | Node::CteScan { cte, .. } =
            *plan.node(u32::try_from(at).unwrap_or(u32::MAX))
        {
            next = next.max(cte + 1);
        }
    }
    next
}

#[cfg(test)]
mod tests {
    use rudb_plan::Plan;

    use super::TotalFromGroups;
    use crate::pass::{Context, Pass};

    /// The shape of TPC-H q11 with `threshold` in the total's filter and `call` as the total.
    fn query(threshold: &str, call: &str) -> String {
        format!(
            "Filter (#3.1::DECIMAL(38,2) > #8.0::DECIMAL(38,2))::BOOLEAN\n  \
               Join SINGLE on=[]\n    \
                 Aggregate #3 groups=[#0.0::BIGINT] aggregates=[sum(#0.2::DECIMAL(15,2))::DECIMAL(38,2), \
                 min(#0.2::DECIMAL(15,2))::DECIMAL(15,2)]\n      \
                   Filter (#0.1::BIGINT > 5::BIGINT)::BOOLEAN\n        \
                     Get memory.main.ps AS ps #0 [k::BIGINT, s::BIGINT, c::DECIMAL(15,2)]\n    \
                 Project #8 [#7.0::DECIMAL(38,2) AS t]\n      \
                   Aggregate #7 groups=[] aggregates=[{call}]\n        \
                     Filter (#4.0::BIGINT > {threshold}::BIGINT)::BOOLEAN\n          \
                       Get memory.main.ps AS ps #4 [s::BIGINT, c::DECIMAL(15,2)]\n"
        )
    }

    fn rewritten(text: &str) -> String {
        let mut plan = Plan::parse(text).expect("a plan the reader accepts");
        TotalFromGroups.run(&mut plan, &Context::default()).expect("the pass runs");
        plan.validate().expect("a valid plan");
        plan.to_string()
    }

    #[test]
    fn a_total_over_the_rows_of_a_grouping_is_added_up_over_its_groups() {
        let text = rewritten(&query("5", "sum(#4.1::DECIMAL(15,2))::DECIMAL(38,2)"));
        assert!(text.starts_with("MaterializedCte groups @0"), "{text}");
        assert!(text.contains("CteScan groups @0 #3"), "{text}");
        assert!(
            text.contains(
                "Aggregate #7 groups=[] aggregates=[sum(#10.1::DECIMAL(38,2))::DECIMAL(38,2)]"
            ),
            "{text}"
        );
        assert!(text.contains("CteScan groups @0 #10"), "{text}");
        assert_eq!(text.matches("Get memory.main.ps").count(), 1, "{text}");
        // A second run has nothing left to do.
        assert_eq!(rewritten(&text), text);
    }

    #[test]
    fn a_smallest_value_is_the_smallest_group_minimum() {
        let text = rewritten(&query("5", "min(#4.1::DECIMAL(15,2))::DECIMAL(15,2)"));
        assert!(text.contains("aggregates=[min(#10.2::DECIMAL(15,2))::DECIMAL(15,2)]"), "{text}");
    }

    #[test]
    fn a_total_over_other_rows_or_of_another_kind_is_left_alone() {
        for (threshold, call) in [
            ("6", "sum(#4.1::DECIMAL(15,2))::DECIMAL(38,2)"),
            ("5", "count(#4.1::DECIMAL(15,2))::BIGINT"),
            ("5", "sum(#4.0::BIGINT)::HUGEINT"),
        ] {
            let text = query(threshold, call);
            let before = Plan::parse(&text).expect("a plan the reader accepts").to_string();
            assert_eq!(rewritten(&text), before, "{call} over {threshold}");
        }
    }
}
