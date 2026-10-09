//! Taking the rows a null key could never join out of the scan, before the joins are ordered.
//!
//! An inner join on `a.x = b.y` keeps no row whose `b.y` is null, because an equality with a null
//! in it is never true. So `b.y IS NOT NULL` holds of every row the join produces, and it can be
//! asked of the scan of `b` rather than of the pairs. Nothing about the answer changes. What
//! changes is how many rows of `b` everything under the join has to carry, and how many rows join
//! ordering thinks `b` brings with it.
//!
//! JOB 10c is the query this is for. Its `cast_info` keeps 1.4 million rows on `note LIKE
//! '%(producer)%'`, it joins `char_name` on `person_role_id`, and a producer plays no role, so
//! almost every one of those rows has a null there. The join to `char_name` keeps ten rows. Without
//! the test the plan built two hash tables of more than a million rows each on the way to it, and
//! with the rule off it was nearly twice as slow as DuckDB.
//!
//! # When it is worth a test
//!
//! The test always holds, so the store is asked only whether it pays, and any count it has will do,
//! an estimate as well as an exact one. A column in which at least one row in `SHARE` is null
//! gets the test, and one with fewer does not, because then the test is a mask read on every row
//! for a handful of rows dropped. A column with no nulls at all is the common case and is left
//! alone, and so is a column the store knows nothing about.
//!
//! # Where this sits in the sequence
//!
//! After filter pushdown, so the filter right above each scan is already there to take the test,
//! and after the consistent rewrite, so a region it answered without a join is not given tests
//! nobody reads. Before join ordering, which is the pass the test is for.
//!
//! The pass looks for the test in the filter above the scan before it adds it, which is what keeps
//! a second run of the sequence from adding it twice.

use std::collections::{BTreeMap, BTreeSet};

use rudb_common::rules::Rule;
use rudb_common::{LogicalType, Result, Value};
use rudb_plan::{
    ColumnBinding, CompareOp, ConjunctionOp, Expr, ExprRef, JoinKind, Node, NodeRef, Plan,
};

use crate::estimate::{self, Facts};
use crate::pass::{Context, Pass};
use crate::walk;

/// Asks the scan under an inner join for the key the join would have thrown away as null.
#[derive(Debug, Clone, Copy)]
pub struct PresentKeys;

/// One row in this many being null is what makes a key column worth a test.
const SHARE: u64 = 100;

impl Pass for PresentKeys {
    fn name(&self) -> &'static str {
        "present_keys"
    }

    fn run(&self, plan: &mut Plan, context: &Context) -> Result<()> {
        if !context.allows(Rule::ValidityFree) {
            return Ok(());
        }
        let found = keyed(plan, context.facts());
        if !found.is_empty() {
            apply(plan, found);
        }
        Ok(())
    }
}

/// The scans an inner join reads a key out of, with the columns of each that are null often enough.
fn keyed(plan: &Plan, facts: &Facts) -> BTreeMap<NodeRef, BTreeSet<u32>> {
    let mut found: BTreeMap<NodeRef, BTreeSet<u32>> = BTreeMap::new();
    for at in reachable(plan) {
        let Node::Join { kind: JoinKind::Inner, conditions, .. } = *plan.node(at) else { continue };
        for &condition in plan.expr_list(conditions) {
            let Expr::Compare { op: CompareOp::Equal, left, right } = *plan.expr(condition) else {
                continue;
            };
            for side in [left, right] {
                let Expr::Column(binding) = *plan.expr(side) else { continue };
                let Some(scan) = walk::scan_of(plan, at, binding.table) else { continue };
                if often_null(plan, scan, binding.column, facts) {
                    found.entry(scan).or_default().insert(binding.column);
                }
            }
        }
    }
    found
}

/// Whether the store guesses at least one row in [`SHARE`] of this column of the scan is null.
fn often_null(plan: &Plan, scan: NodeRef, column: u32, facts: &Facts) -> bool {
    let Some(nulls) = estimate::nulls_guessed(plan, scan, column as usize) else { return false };
    let Some(&rows) = estimate::rows_stat(plan, scan, facts).value() else { return false };
    nulls > 0 && nulls.saturating_mul(SHARE) >= rows
}

/// Every node the root reaches, each once.
fn reachable(plan: &Plan) -> Vec<NodeRef> {
    let mut seen = vec![false; plan.node_count()];
    let mut stack = vec![plan.root()];
    let mut found = Vec::new();
    while let Some(at) = stack.pop() {
        let Some(slot) = seen.get_mut(at as usize) else { continue };
        if *slot {
            continue;
        }
        *slot = true;
        found.push(at);
        stack.extend(plan.node(at).children().into_iter().flatten());
    }
    found
}

/// Writes the tests in: into the filter right above each scan where there is one, and into a new
/// one where there is not, leaving out a test the filter already asks.
fn apply(plan: &mut Plan, found: BTreeMap<NodeRef, BTreeSet<u32>>) {
    let above = filters_above(plan);
    let mut bare = BTreeMap::new();
    for (scan, columns) in found {
        let Node::Get { index, columns: fields, .. } = *plan.node(scan) else { continue };
        let held = above.get(&scan).map(|&filter| match *plan.node(filter) {
            Node::Filter { predicate, .. } => (filter, predicate),
            _ => unreachable!("only filters are recorded"),
        });
        let asked = held.map(|(_, predicate)| conjuncts(plan, predicate)).unwrap_or_default();
        let mut tests = Vec::new();
        for column in columns {
            let Some(field) = plan.field_list(fields).get(column as usize) else { continue };
            let ty = field.ty.clone();
            let test = present(plan, ColumnBinding::new(index, column), ty);
            if !asked.iter().any(|&part| walk::same(plan, part, test)) {
                tests.push(test);
            }
        }
        if tests.is_empty() {
            continue;
        }
        match held {
            Some((filter, predicate)) => {
                let predicate = conjoined(plan, Some(predicate), &tests);
                if let Node::Filter { predicate: held, .. } = plan.node_mut(filter) {
                    *held = predicate;
                }
            }
            None => {
                bare.insert(scan, tests);
            }
        }
    }
    if bare.is_empty() {
        return;
    }
    let mut changed = false;
    let root = plan.root();
    let rebuilt = walk::restack(plan, root, &mut changed, &mut |plan, at| {
        let tests = bare.remove(&at)?;
        let predicate = conjoined(plan, None, &tests);
        let span = plan.node_span(at);
        Some(plan.add_node_at(Node::Filter { input: at, predicate }, span))
    });
    if changed {
        plan.set_root(rebuilt);
    }
}

/// The filter each scan sits right under, for the scans that sit under one.
fn filters_above(plan: &Plan) -> BTreeMap<NodeRef, NodeRef> {
    let mut above = BTreeMap::new();
    for at in reachable(plan) {
        if let Node::Filter { input, .. } = *plan.node(at)
            && matches!(plan.node(input), Node::Get { .. })
        {
            above.insert(input, at);
        }
    }
    above
}

/// `column IS NOT NULL`, written the way the binder writes it, as `IS DISTINCT FROM NULL`.
fn present(plan: &mut Plan, column: ColumnBinding, ty: LogicalType) -> ExprRef {
    let left = plan.add_expr(Expr::Column(column), ty.clone());
    let null = plan.add_value(Value::Null);
    let right = plan.add_expr(Expr::Constant(null), ty);
    let op = CompareOp::DistinctFrom;
    plan.add_expr(Expr::Compare { op, left, right }, LogicalType::Boolean)
}

/// The parts of a conjunction, or the predicate itself when it is not one.
fn conjuncts(plan: &Plan, predicate: ExprRef) -> Vec<ExprRef> {
    match *plan.expr(predicate) {
        Expr::Conjunction { op: ConjunctionOp::And, children } => plan.expr_list(children).to_vec(),
        _ => vec![predicate],
    }
}

/// `held AND tests`, flattened into one conjunction.
fn conjoined(plan: &mut Plan, held: Option<ExprRef>, tests: &[ExprRef]) -> ExprRef {
    let mut all = Vec::with_capacity(tests.len() + 2);
    if let Some(held) = held {
        all.extend(conjuncts(plan, held));
    }
    all.extend_from_slice(tests);
    if let [only] = all[..] {
        return only;
    }
    let children = plan.add_expr_list(&all);
    plan.add_expr(Expr::Conjunction { op: ConjunctionOp::And, children }, LogicalType::Boolean)
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use rudb_common::Stat;
    use rudb_common::bounds::{Bound, End, Spread, Test, Zones};
    use rudb_common::rules::{Rule, Rules};
    use rudb_common::stat::Provenance;
    use rudb_plan::Plan;

    use super::PresentKeys;
    use crate::estimate::Facts;
    use crate::pass::{Context, Pass};

    /// A store of `cast_info` whose `person_role_id` holds this many nulls.
    #[derive(Debug)]
    struct Roles(u64);

    impl Zones for Roles {
        fn column(&self, name: &str) -> Option<usize> {
            (name == "person_role_id").then_some(1)
        }

        fn surviving(&self, _tests: &[Test]) -> Option<u64> {
            None
        }

        fn spread(&self, _tests: &[Test]) -> Option<Spread> {
            None
        }

        fn extreme(&self, _column: usize, _end: End) -> Stat<Bound> {
            Stat::Unknown
        }

        fn nulls(&self, _column: usize) -> Stat<u64> {
            Stat::estimated(self.0, Provenance::NullCount)
        }
    }

    /// 10c's last join, with `cast_info` filtered on its note and `char_name` bare.
    const JOINED: &str = concat!(
        "Join INNER on=[(#0.1::INTEGER = #1.0::INTEGER)::BOOLEAN]\n",
        "  Filter (#0.0::INTEGER > 5::INTEGER)::BOOLEAN\n",
        "    Get memory.main.cast_info AS ci #0 [note::INTEGER, person_role_id::INTEGER]\n",
        "  Get memory.main.char_name AS chn #1 [id::INTEGER]\n"
    );

    const TESTED: &str = "(#0.1::INTEGER IS DISTINCT FROM NULL::INTEGER)";

    fn run(text: &str, nulls: u64, context: &mut Context) -> String {
        let mut plan =
            Plan::parse(text).unwrap_or_else(|error| panic!("{text} did not parse: {error}"));
        plan.set_zones(0, Arc::new(Roles(nulls)) as Arc<dyn Zones>);
        let mut facts = Facts::new();
        facts.record("memory", "main", "cast_info", 1_000_000);
        facts.record("memory", "main", "char_name", 3_000_000);
        context.measure(Arc::new(facts));
        PresentKeys.run(&mut plan, context).expect("the pass does not fail");
        plan.validate().expect("the plan is still valid");
        let once = plan.to_string();
        PresentKeys.run(&mut plan, context).expect("the pass does not fail");
        assert_eq!(plan.to_string(), once, "a second run changed the plan");
        once
    }

    #[test]
    fn a_key_that_is_mostly_null_is_tested_at_the_scan() {
        let text = run(JOINED, 900_000, &mut Context::new());
        assert!(
            text.contains(&format!("(#0.0::INTEGER > 5::INTEGER)::BOOLEAN AND {TESTED}")),
            "the test is not in the filter over the scan:\n{text}"
        );
    }

    #[test]
    fn a_bare_scan_is_given_a_filter_of_its_own() {
        let bare =
            JOINED.replace("  Filter (#0.0::INTEGER > 5::INTEGER)::BOOLEAN\n    Get", "  Get");
        let text = run(&bare, 900_000, &mut Context::new());
        assert!(text.contains(&format!("Filter {TESTED}")), "no filter over the scan:\n{text}");
    }

    #[test]
    fn a_key_that_is_rarely_null_is_left_alone() {
        let text = run(JOINED, 10, &mut Context::new());
        assert!(!text.contains("IS DISTINCT FROM"), "a test for ten rows was added:\n{text}");
    }

    #[test]
    fn an_outer_join_keeps_its_nulls() {
        let left = JOINED.replace("Join INNER", "Join LEFT");
        let text = run(&left, 900_000, &mut Context::new());
        assert!(!text.contains("IS DISTINCT FROM"), "a left join lost its null keys:\n{text}");
    }

    #[test]
    fn the_rule_turns_it_off() {
        let mut context = Context::new();
        let mut rules = Rules::new();
        rules.set(Rule::ValidityFree, false);
        context.govern(rules);
        let text = run(JOINED, 900_000, &mut context);
        assert!(!text.contains("IS DISTINCT FROM"), "the rule was off:\n{text}");
    }
}
