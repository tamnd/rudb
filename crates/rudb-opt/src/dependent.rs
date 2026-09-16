//! Removes group keys whose value is determined by other group keys.
//!
//! `GROUP BY ip, ip - 1, ip - 2` has one independent key. Hashing all three values for every
//! input row makes the hash table wider without separating additional rows. This pass groups by
//! `ip` and computes the dependent expressions once for each output group.

use std::collections::HashMap;

use rudb_common::Result;
use rudb_plan::{ColumnBinding, Expr, ExprRef, Node, NodeRef, Plan};

use crate::pass::{Context, Pass};
use crate::walk;

#[derive(Debug, Clone, Copy)]
pub struct DependentGroupKeys;

impl Pass for DependentGroupKeys {
    fn name(&self) -> &'static str {
        "dependent_group_keys"
    }

    fn run(&self, plan: &mut Plan, _context: &Context) -> Result<()> {
        remove(plan);
        Ok(())
    }
}

fn remove(plan: &mut Plan) {
    let mut changed = false;
    let root = walk::restack(plan, plan.root(), &mut changed, &mut rewrite);
    if changed {
        plan.set_root(root);
    }
}

fn rewrite(plan: &mut Plan, at: NodeRef) -> Option<NodeRef> {
    let Node::Aggregate { input, index, groups, aggregates } = *plan.node(at) else { return None };
    let keys = plan.expr_list(groups).to_vec();
    if keys.len() < 2 {
        return None;
    }

    let bases: HashMap<ColumnBinding, usize> = keys
        .iter()
        .enumerate()
        .filter_map(|(position, &key)| match *plan.expr(key) {
            Expr::Column(binding) => Some((binding, position)),
            _ => None,
        })
        .collect();
    if bases.is_empty() {
        return None;
    }

    let dependent: Vec<bool> = keys
        .iter()
        .map(|&key| {
            if matches!(plan.expr(key), Expr::Column(_)) || walk::volatile(plan, key) {
                return false;
            }
            let mut saw_column = false;
            let mut covered = true;
            walk::columns(plan, key, &mut |binding| {
                saw_column = true;
                covered &= bases.contains_key(&binding);
            });
            saw_column && covered && walk::elementwise(plan, key)
        })
        .collect();
    if !dependent.iter().any(|&yes| yes) {
        return None;
    }

    let kept: Vec<ExprRef> =
        keys.iter().zip(&dependent).filter_map(|(&key, &drop)| (!drop).then_some(key)).collect();
    let staged = walk::fresh_index(plan);
    let kept_slice = plan.add_expr_list(&kept);
    let inner =
        plan.add_node(Node::Aggregate { input, index: staged, groups: kept_slice, aggregates });

    let mut base_outputs = HashMap::new();
    for (output, &key) in kept.iter().enumerate() {
        if let Expr::Column(binding) = *plan.expr(key) {
            base_outputs.insert(binding, output);
        }
    }

    let mut projected = Vec::with_capacity(keys.len() + plan.expr_list(aggregates).len());
    for (&key, &drop) in keys.iter().zip(&dependent) {
        if drop {
            projected.push(replay(plan, key, staged, &base_outputs));
        } else {
            let position = kept.iter().position(|&held| held == key)?;
            projected.push(column(plan, staged, position, key));
        }
    }
    let calls = plan.expr_list(aggregates).to_vec();
    for (offset, &call) in calls.iter().enumerate() {
        projected.push(column(plan, staged, kept.len() + offset, call));
    }

    let names: Vec<_> =
        (0..projected.len()).map(|position| plan.intern(&format!("column{position}"))).collect();
    let exprs = plan.add_expr_list(&projected);
    let names = plan.add_name_list(&names);
    Some(plan.add_node(Node::Project { input: inner, index, exprs, names }))
}

fn column(plan: &mut Plan, table: u32, position: usize, source: ExprRef) -> ExprRef {
    let position = u32::try_from(position).expect("an aggregate cannot have this many columns");
    let ty = plan.expr_type(source).clone();
    let span = plan.expr_span(source);
    plan.add_expr_at(Expr::Column(ColumnBinding::new(table, position)), ty, span)
}

fn replay(
    plan: &mut Plan,
    expr: ExprRef,
    table: u32,
    outputs: &HashMap<ColumnBinding, usize>,
) -> ExprRef {
    if let Expr::Column(binding) = *plan.expr(expr) {
        return column(plan, table, outputs[&binding], expr);
    }
    walk::rebuild(plan, expr, &mut |plan, child| replay(plan, child, table, outputs))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rewritten(text: &str) -> String {
        let mut plan =
            Plan::parse(text).unwrap_or_else(|error| panic!("{text} did not parse: {error}"));
        remove(&mut plan);
        plan.validate().unwrap_or_else(|error| panic!("{text} did not stay valid: {error}"));
        plan.to_string()
    }

    #[test]
    fn expressions_of_a_bare_group_key_are_computed_after_aggregation() {
        let before = "Aggregate #1 groups=[#0.0::INTEGER, \"-\"(#0.0::INTEGER, 1::INTEGER)::INTEGER, \"-\"(#0.0::INTEGER, 2::INTEGER)::INTEGER] aggregates=[count_star()::BIGINT]\n  Get memory.main.t AS t #0 [a::INTEGER]\n";
        let after = rewritten(before);
        assert!(after.contains("Project #1"), "{after}");
        assert!(after.contains("Aggregate #2 groups=[#0.0::INTEGER]"), "{after}");
        assert!(after.contains("\"-\"(#2.0::INTEGER, 1::INTEGER)"), "{after}");
    }

    #[test]
    fn an_expression_with_an_independent_column_stays_in_the_hash_key() {
        let before = "Aggregate #1 groups=[#0.0::INTEGER, \"+\"(#0.0::INTEGER, #0.1::INTEGER)::INTEGER] aggregates=[]\n  Get memory.main.t AS t #0 [a::INTEGER, b::INTEGER]\n";
        assert_eq!(rewritten(before), before);
    }
}
