//! Carrying a date test across a relationship, onto the side a join builds.
//!
//! `spec/stats/07-graph-statistics.md` section 7.9. A link's build measures, for each pair of date
//! columns one on each side, the smallest and the largest `child - parent` over the rows it linked.
//! A join over that relationship then knows more than either filter says: TPC-H Q3 keeps lines
//! shipped after 1995-03-15 and orders placed before it, and a line ships at most 121 days after its
//! order, so the only orders that can meet a kept line were placed after 1994-11-13. That is a test
//! on `o_orderdate`, which the file's zone maps and the join's build can both use, and on SF1 it
//! takes the orders the join holds from 147,000 to about 15,000.
//!
//! # Only onto the build side
//!
//! A derived test is written on the input a join builds and nowhere else. The build side is the one
//! the join holds whole, so every row taken off it is memory and hashing saved, and it is the side a
//! sideways handoff reads its keys from, so the probe side reads fewer rows too. A test carried onto
//! the probe side saves a lookup per row it drops and costs a comparison per row it reads, and the
//! probe side is usually a scan the handoff has already narrowed, so it is left alone. That also
//! keeps the pass out of join ordering: it runs after the plan's shape is fixed and only narrows it.
//!
//! # Why it is sound
//!
//! Take a join whose condition includes `child.fk = parent.pk` for a relationship with a built link.
//! A row of the build side that meets a row of the other side meets it on that equality, so the two
//! are a child and its parent, and their dates are a linked pair the span covers. So a build row
//! whose date is outside what the span allows given the other side's filters meets no row that
//! survives them, and dropping it changes nothing an inner or a semi join returns. An anti join
//! returns the left rows that met nothing and a left join pads them, so for those two dropping build
//! rows that meet nothing is sound when the build side is the right input and not otherwise.
//!
//! Between the join and the build side's scan there can only be operators that pass a row through
//! or drop it: a filter, an inner join, the kept input of a semi, anti or left join, and the child
//! of a link join. Anything else would let a dropped row change some other row.
//!
//! A null decides which way a span may be carried, and the link's build records both answers, see
//! `rudb_graph::span`.

use std::collections::BTreeMap;

use rudb_common::bounds::{Bound, Op};
use rudb_common::rules::Rule;
use rudb_common::{Field, LogicalType, Result, Value};
use rudb_plan::{
    BuildSide, ColumnBinding, CompareOp, ConjunctionOp, Expr, ExprRef, JoinKind, Node, NodeRef,
    Plan,
};

use crate::bounds;
use crate::link::Linked;
use crate::pass::{Context, Pass};
use crate::walk;

/// Carries date tests across relationships whose links measured a span.
#[derive(Debug, Clone, Copy, Default)]
pub struct LinkSpans;

/// How many times the walk runs. A test carried onto one join's build side can be the filter
/// another join carries further, and each round carries it one join, which is as far as TPC-H's
/// three dated tables need.
const ROUNDS: usize = 3;

impl Pass for LinkSpans {
    fn name(&self) -> &'static str {
        "link_spans"
    }

    fn run(&self, plan: &mut Plan, context: &Context) -> Result<()> {
        if !context.allows(Rule::GraphReduction)
            || !context.links().iter().any(|linked| linked.built && !linked.spans.is_empty())
        {
            return Ok(());
        }
        for _ in 0..ROUNDS {
            let found = derived(plan, context.links());
            if found.is_empty() {
                break;
            }
            apply(plan, found);
        }
        Ok(())
    }
}

/// A range of days, both ends kept, either end open.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Range {
    low: Option<i128>,
    high: Option<i128>,
}

impl Range {
    const ALL: Self = Self { low: None, high: None };

    /// The range with one more test applied.
    fn and(self, op: Op, value: i128) -> Self {
        let raise = |low: Option<i128>, to: i128| Some(low.map_or(to, |low| low.max(to)));
        let lower = |high: Option<i128>, to: i128| Some(high.map_or(to, |high| high.min(to)));
        match op {
            Op::Equal => Self { low: raise(self.low, value), high: lower(self.high, value) },
            Op::Greater => Self { low: raise(self.low, value.saturating_add(1)), ..self },
            Op::GreaterOrEqual => Self { low: raise(self.low, value), ..self },
            Op::Less => Self { high: lower(self.high, value.saturating_sub(1)), ..self },
            Op::LessOrEqual => Self { high: lower(self.high, value), ..self },
        }
    }

    /// Both ranges at once.
    fn meet(self, other: Self) -> Self {
        let low = match (self.low, other.low) {
            (Some(one), Some(two)) => Some(one.max(two)),
            (one, two) => one.or(two),
        };
        let high = match (self.high, other.high) {
            (Some(one), Some(two)) => Some(one.min(two)),
            (one, two) => one.or(two),
        };
        Self { low, high }
    }

    /// The ends of this range that are tighter than `held`, and nothing where `held` is as tight.
    fn beyond(self, held: Self) -> Self {
        Self {
            low: self.low.filter(|&low| held.low.is_none_or(|held| low > held)),
            high: self.high.filter(|&high| held.high.is_none_or(|held| high < held)),
        }
    }
}

/// One test to add: the scan it goes above, and the column and the range it keeps.
#[derive(Debug)]
struct Derived {
    scan: NodeRef,
    column: String,
    range: Range,
}

/// Every test the joins of the plan let a span carry that the build side does not already have.
fn derived(plan: &Plan, links: &[Linked]) -> Vec<Derived> {
    let mut found = Vec::new();
    for at in reachable(plan) {
        let Node::Join { left, right, kind, conditions, build } = *plan.node(at) else {
            continue;
        };
        let (target, source) = match build {
            BuildSide::Right => (right, left),
            BuildSide::Left => (left, right),
        };
        match kind {
            JoinKind::Inner | JoinKind::Semi => {}
            JoinKind::Anti | JoinKind::Left if build == BuildSide::Right => {}
            _ => continue,
        }
        for &condition in plan.expr_list(conditions) {
            let Expr::Compare { op: CompareOp::Equal, left: one, right: two } =
                *plan.expr(condition)
            else {
                continue;
            };
            let (&Expr::Column(one), &Expr::Column(two)) = (plan.expr(one), plan.expr(two)) else {
                continue;
            };
            let sides =
                match (path(plan, target, one.table, true), path(plan, source, two.table, false)) {
                    (Some(target), Some(source)) => Some((target, one, source, two)),
                    _ => match (
                        path(plan, target, two.table, true),
                        path(plan, source, one.table, false),
                    ) {
                        (Some(target), Some(source)) => Some((target, two, source, one)),
                        _ => None,
                    },
                };
            let Some((target, target_key, source, source_key)) = sides else { continue };
            across(plan, links, (&target, target_key), (&source, source_key), &mut found);
        }
    }
    found
}

/// The tests one equality carries onto its build side, pushed onto `found`.
///
/// Each path runs from the join down to the scan, so the scan is its last node.
fn across(
    plan: &Plan,
    links: &[Linked],
    target: (&[NodeRef], ColumnBinding),
    source: (&[NodeRef], ColumnBinding),
    found: &mut Vec<Derived>,
) {
    let (Some(&target_scan), Some(&source_scan)) = (target.0.last(), source.0.last()) else {
        return;
    };
    let (Some((target_table, target_fields)), Some((source_table, source_fields))) =
        (table(plan, target_scan), table(plan, source_scan))
    else {
        return;
    };
    let (Some(target_key), Some(source_key)) =
        (target_fields.get(target.1.column as usize), source_fields.get(source.1.column as usize))
    else {
        return;
    };
    let same = |one: &str, two: &str| one.eq_ignore_ascii_case(two);
    for linked in links.iter().filter(|linked| linked.built && linked.second.is_none()) {
        let onto_parent = same(&linked.child, source_table)
            && same(&linked.child_column, &source_key.name)
            && same(&linked.parent, target_table)
            && same(&linked.parent_column, &target_key.name);
        let onto_child = same(&linked.child, target_table)
            && same(&linked.child_column, &target_key.name)
            && same(&linked.parent, source_table)
            && same(&linked.parent_column, &source_key.name);
        if !onto_parent && !onto_child {
            continue;
        }
        for span in &linked.spans {
            let (from, onto, allowed) = if onto_parent {
                (&span.child_column, &span.parent_column, span.onto_parent)
            } else {
                (&span.parent_column, &span.child_column, span.onto_child)
            };
            if !allowed || !dated(target_fields, onto, true) || !dated(source_fields, from, false) {
                continue;
            }
            let kept = range(plan, source.0, source_fields, from);
            if kept == Range::ALL {
                continue;
            }
            // `child - parent` is between low and high, so a parent is its child less one of those
            // and a child is its parent plus one of them.
            let (low, high) = (i128::from(span.low), i128::from(span.high));
            let carried = if onto_parent {
                Range { low: kept.low.map(|at| at - high), high: kept.high.map(|at| at - low) }
            } else {
                Range { low: kept.low.map(|at| at + low), high: kept.high.map(|at| at + high) }
            };
            let fresh = carried.beyond(range(plan, target.0, target_fields, onto));
            if fresh != Range::ALL {
                found.push(Derived { scan: target_scan, column: onto.clone(), range: fresh });
            }
        }
    }
}

/// Whether a scan's fields hold this column as a date, or, where `absent` is allowed, do not hold
/// it at all, which is a column the scan can be widened to read.
fn dated(fields: &[Field], column: &str, absent: bool) -> bool {
    match fields.iter().find(|field| field.name.eq_ignore_ascii_case(column)) {
        Some(field) => field.ty == LogicalType::Date,
        None => absent,
    }
}

/// The table a scan reads and the fields it produces.
fn table(plan: &Plan, scan: NodeRef) -> Option<(&str, &[Field])> {
    match *plan.node(scan) {
        Node::Get { table, columns, .. } => Some((plan.string(table), plan.field_list(columns))),
        _ => None,
    }
}

/// The days a column of the scan at the end of `path` is held to by the filters along it.
fn range(plan: &Plan, path: &[NodeRef], fields: &[Field], column: &str) -> Range {
    let Some(&scan) = path.last() else { return Range::ALL };
    let Some(position) = fields.iter().position(|field| field.name.eq_ignore_ascii_case(column))
    else {
        return Range::ALL;
    };
    let mut kept = Range::ALL;
    for &node in path {
        let Node::Filter { predicate, .. } = *plan.node(node) else { continue };
        for (at, op, bound) in bounds::of(plan, scan, predicate) {
            if let (true, Bound::Int(value)) = (at == position, bound) {
                kept = kept.and(op, value);
            }
        }
    }
    kept
}

/// The nodes from `at` down to the scan numbered `index`, through operators that keep a row as it
/// was or drop it, or nothing when the scan is not below `at` that way.
///
/// The build side, `target`, is held to what the module doc says. The other side only has to carry
/// the scan's rows up with their columns intact, since a row a join pads has a null key and meets
/// nothing.
fn path(plan: &Plan, at: NodeRef, index: u32, target: bool) -> Option<Vec<NodeRef>> {
    let below = match *plan.node(at) {
        Node::Get { index: found, .. } if found == index => return Some(vec![at]),
        Node::Filter { input, .. } | Node::LinkJoin { child: input, .. } => {
            path(plan, input, index, target)
        }
        Node::Join { left, right, kind, .. } => {
            let both = match kind {
                JoinKind::Inner => true,
                JoinKind::Semi | JoinKind::Anti => false,
                JoinKind::Left if target => false,
                JoinKind::Left | JoinKind::Right | JoinKind::Full | JoinKind::Single if !target => {
                    true
                }
                _ => return None,
            };
            path(plan, left, index, target)
                .or_else(|| both.then(|| path(plan, right, index, target)).flatten())
        }
        _ => None,
    }?;
    let mut below = below;
    below.insert(0, at);
    Some(below)
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

/// Writes the tests into the plan: into the filter right above each scan where there is one, and in
/// a new one where there is not.
fn apply(plan: &mut Plan, found: Vec<Derived>) {
    let mut by_scan: BTreeMap<NodeRef, BTreeMap<String, Range>> = BTreeMap::new();
    for Derived { scan, column, range } in found {
        let columns = by_scan.entry(scan).or_default();
        let held = columns.entry(column).or_insert(Range::ALL);
        *held = held.meet(range);
    }
    let above = filters_above(plan);
    let mut bare = BTreeMap::new();
    for (scan, columns) in by_scan {
        let mut tests = Vec::new();
        for (column, range) in columns {
            let Some(binding) = widened(plan, scan, &column) else { continue };
            tests.extend(compare(plan, binding, CompareOp::GreaterOrEqual, range.low));
            tests.extend(compare(plan, binding, CompareOp::LessOrEqual, range.high));
        }
        if tests.is_empty() {
            continue;
        }
        match above.get(&scan) {
            Some(&filter) => {
                let Node::Filter { predicate, .. } = *plan.node(filter) else { continue };
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

/// The binding of a column of a scan, with the scan widened to read it when it did not.
///
/// A column only a derived test reads is carried up past the scan like any other, which costs a
/// column of dates and is what lets the file skip the blocks the test rules out.
fn widened(plan: &mut Plan, scan: NodeRef, column: &str) -> Option<ColumnBinding> {
    let Node::Get { index, columns, .. } = *plan.node(scan) else { return None };
    let mut fields = plan.field_list(columns).to_vec();
    if let Some(at) = fields.iter().position(|field| field.name.eq_ignore_ascii_case(column)) {
        return Some(ColumnBinding::new(index, u32::try_from(at).ok()?));
    }
    let at = u32::try_from(fields.len()).ok()?;
    fields.push(Field::new(column.to_string(), LogicalType::Date));
    let widened = plan.add_fields(&fields);
    match plan.node_mut(scan) {
        Node::Get { columns, .. } => *columns = widened,
        _ => return None,
    }
    Some(ColumnBinding::new(index, at))
}

/// `column op day`, or nothing for an open end or a day no `DATE` holds.
fn compare(
    plan: &mut Plan,
    column: ColumnBinding,
    op: CompareOp,
    day: Option<i128>,
) -> Option<ExprRef> {
    let day = i32::try_from(day?).ok()?;
    let left = plan.add_expr(Expr::Column(column), LogicalType::Date);
    let value = plan.add_value(Value::Date(day));
    let right = plan.add_expr(Expr::Constant(value), LogicalType::Date);
    Some(plan.add_expr(Expr::Compare { op, left, right }, LogicalType::Boolean))
}

/// `held AND tests`, flattened into one conjunction.
fn conjoined(plan: &mut Plan, held: Option<ExprRef>, tests: &[ExprRef]) -> ExprRef {
    let mut all = Vec::with_capacity(tests.len() + 2);
    if let Some(held) = held {
        match *plan.expr(held) {
            Expr::Conjunction { op: ConjunctionOp::And, children } => {
                all.extend_from_slice(plan.expr_list(children));
            }
            _ => all.push(held),
        }
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

    use rudb_common::rules::{Rule, Rules};
    use rudb_plan::Plan;

    use super::LinkSpans;
    use crate::link::{Linked, Span};
    use crate::pass::{Context, Pass};

    /// `lineitem -> orders` with TPC-H's ship date span, 1 to 121 days, usable both ways.
    fn context(onto_child: bool) -> Context {
        let span = Span {
            child_column: "l_shipdate".to_string(),
            parent_column: "o_orderdate".to_string(),
            low: 1,
            high: 121,
            onto_parent: true,
            onto_child,
        };
        let linked =
            Linked::built("lineitem", "l_orderkey", "orders", "o_orderkey").spanned(vec![span]);
        let mut context = Context::new();
        context.relate(Arc::new(vec![linked]));
        context
    }

    fn run(text: &str, context: &Context) -> String {
        let mut plan =
            Plan::parse(text).unwrap_or_else(|error| panic!("{text} did not parse: {error}"));
        LinkSpans.run(&mut plan, context).expect("the pass does not fail");
        plan.validate().expect("the plan is still valid");
        plan.to_string()
    }

    /// Q3's top join: lines shipped after day 9204 probe orders placed before it.
    fn q3(build: &str, orders: &str) -> String {
        format!(
            "Join INNER on=[(#0.0::BIGINT = #1.0::BIGINT)::BOOLEAN] build={build}\n  \
             Filter (#0.1::DATE > 9204::DATE)::BOOLEAN\n    \
             Get memory.main.lineitem AS lineitem #0 [l_orderkey::BIGINT, l_shipdate::DATE]\n  \
             {orders}"
        )
    }

    #[test]
    fn a_ship_date_test_narrows_the_orders_a_join_builds() {
        let orders = "Filter (#1.1::DATE < 9204::DATE)::BOOLEAN\n    \
                      Get memory.main.orders AS orders #1 [o_orderkey::BIGINT, o_orderdate::DATE]\n";
        let text = run(&q3("right", orders), &context(true));
        assert!(
            text.contains("(#1.1::DATE >= 9084::DATE)"),
            "no lower bound on the orders:\n{text}"
        );
        assert!(!text.contains("#0.1::DATE <="), "the probe side was narrowed:\n{text}");
    }

    #[test]
    fn an_unprojected_date_is_read_so_it_can_be_tested() {
        let orders = "Get memory.main.orders AS orders #1 [o_orderkey::BIGINT]\n";
        let text = run(&q3("right", orders), &context(true));
        assert!(text.contains("o_orderdate::DATE"), "the scan was not widened:\n{text}");
        assert!(text.contains("(#1.1::DATE >= 9084::DATE)"), "no test on the orders:\n{text}");
    }

    #[test]
    fn the_probe_side_is_left_alone() {
        let orders = "Filter (#1.1::DATE < 9204::DATE)::BOOLEAN\n    \
                      Get memory.main.orders AS orders #1 [o_orderkey::BIGINT, o_orderdate::DATE]\n";
        let text = run(&q3("left", orders), &context(true));
        assert!(!text.contains(">= 9084"), "the orders were narrowed from the probe side:\n{text}");
        assert!(
            text.contains("(#0.1::DATE <= 9324::DATE)"),
            "the lines were not narrowed:\n{text}"
        );
    }

    #[test]
    fn a_child_date_that_may_be_null_is_not_narrowed() {
        let orders = "Filter (#1.1::DATE < 9204::DATE)::BOOLEAN\n    \
                      Get memory.main.orders AS orders #1 [o_orderkey::BIGINT, o_orderdate::DATE]\n";
        let text = run(&q3("left", orders), &context(false));
        assert!(!text.contains("<= 9324"), "a null ship date would have been dropped:\n{text}");
    }

    #[test]
    fn a_test_already_as_tight_is_not_added_again() {
        let orders = "Filter (#1.1::DATE >= 9100::DATE)::BOOLEAN\n    \
                      Get memory.main.orders AS orders #1 [o_orderkey::BIGINT, o_orderdate::DATE]\n";
        let text = run(&q3("right", orders), &context(true));
        assert!(!text.contains("9084"), "a looser test was added:\n{text}");
    }

    #[test]
    fn a_right_join_does_not_narrow_the_side_it_keeps() {
        let text = "Join RIGHT on=[(#0.0::BIGINT = #1.0::BIGINT)::BOOLEAN]\n  \
                    Filter (#0.1::DATE > 9204::DATE)::BOOLEAN\n    \
                    Get memory.main.lineitem AS lineitem #0 [l_orderkey::BIGINT, l_shipdate::DATE]\n  \
                    Get memory.main.orders AS orders #1 [o_orderkey::BIGINT, o_orderdate::DATE]\n";
        let text = run(text, &context(true));
        assert!(!text.contains("9084"), "a right join's kept side was narrowed:\n{text}");
    }

    #[test]
    fn the_rule_turns_it_off() {
        let mut context = context(true);
        let mut rules = Rules::new();
        rules.set(Rule::GraphReduction, false);
        context.govern(rules);
        let orders = "Get memory.main.orders AS orders #1 [o_orderkey::BIGINT]\n";
        let text = run(&q3("right", orders), &context);
        assert!(!text.contains("o_orderdate"), "the rule was off:\n{text}");
    }
}
