//! Reading a filter as tests a store's minimum and maximum can answer.
//!
//! Two callers want the same reading of the same predicate and they want it for different reasons.
//! The physical builder wants it so a scan can step over a row group without touching a row of it.
//! The estimator wants it so the row count above a filter is the rows in the parts that survive
//! rather than a fraction of the whole file. Both are asking which conjuncts are a column compared
//! against a constant, and both have to get the direction of the comparison right.
//!
//! So it is read once, here. Two copies of this would be two chances to flip one operator, and the
//! failure that causes is not a slow query in either caller: in the builder it is a row group
//! skipped that held rows the query wanted, and in the estimator it is a count below the truth that
//! a later pass then builds a plan on. A wrong answer, arrived at quickly.
//!
//! This is rank eleven and the builder is rank twelve, so the builder reads it here and nothing
//! moves the other way. The vocabulary itself, [`Op`] and [`Bound`], is at rank zero, because a
//! Parquet row group, a chunk of a table in memory and a block of the storage format all keep the
//! same pair of values and all get asked the same question.
//!
//! # Only an AND, and only a constant
//!
//! A conjunct under an `OR` says nothing about the row when it is false, so only an `AND` is walked
//! into, and a `NOT` is gone by the time the plan is bound. A comparison of two columns is not here
//! either: the bounds of one column say nothing about the other's value in the same row.
//!
//! Every conjunct that does not read as a test is dropped, which is the conservative direction. A
//! test that is missing costs time in the builder and an estimate that is too large in the
//! estimator, and neither of those is a wrong answer.

use rudb_common::bounds::{Bound, Op};
use rudb_plan::{ColumnBinding, CompareOp, ConjunctionOp, Expr, ExprRef, Node, Plan};

/// One conjunct read as a test, with the column named the way the plan names it.
///
/// The position is into what the scan produces and not into the file, because that is what a
/// [`ColumnBinding`] holds and this is reading a plan. A caller that needs the file's own numbering
/// turns the position into a name and asks the file, which is what column pruning makes necessary:
/// pruning moves the position and moves nothing else.
pub type Test = (usize, Op, Bound);

/// What a filter over a scan can tell that scan before it reads anything.
///
/// `input` is what the filter sits on. Only a scan has bounds to test, so anything else answers
/// with nothing, and of the table functions only `read_parquet` is worth it because a CSV has no
/// footer. Only a comparison against that scan's own columns counts, since a binding into some
/// other operator's output is not in this table at all.
#[must_use]
pub fn of(plan: &Plan, input: rudb_plan::NodeRef, predicate: ExprRef) -> Vec<Test> {
    let Some(index) = scanned(plan, input) else { return Vec::new() };
    let mut tests = Vec::new();
    conjuncts(plan, predicate, index, &mut tests);
    tests
}

/// A filter that a stored table below it applies itself, and what the table gets to know about it.
///
/// The predicate is not in here, because the caller is holding the filter node and already has it.
/// What is in here is the part of it a zone map can answer, and whether that part is the whole
/// thing. The two are separate questions and the second one is the dangerous one: a zone that passes
/// every test proves every row passes only when the tests are the whole predicate, so a scan that
/// reads `whole` as true when it is not waves rows through that the query wanted thrown away.
#[derive(Debug, Clone)]
pub struct Moved {
    /// The conjuncts that read as tests, in the numbering of what the scan produces.
    ///
    /// Empty when nothing read, which is a filter the scan applies with no help from the zone maps.
    pub tests: Vec<Test>,
    /// Whether `tests` is the whole of the predicate.
    pub whole: bool,
    /// For each operand of a top level `AND`, in the order the predicate lists them, the tests that
    /// are the whole of that operand, or `None` for an operand that does not read as tests.
    ///
    /// Empty when the predicate is not an `AND`. This is what lets a scan leave out one conjunct on
    /// a stretch whose bounds prove it, when the stretch cannot prove the rest. ClickBench 42 reads
    /// only parts where the counter and the date hold on every row, and compared both on every row
    /// anyway because its two flag columns could not be settled the same way.
    pub conjuncts: Vec<Option<Vec<Test>>>,
}

/// The filter a stored table directly below it can apply itself instead of having one above it.
///
/// Asked of the filter node rather than of a predicate and an input, because two callers ask it and
/// neither of them should be deciding it. The builder asks so it can move the filter into the scan
/// and build no operator for it. `EXPLAIN` asks so the filter's line can say that its work happened
/// below it, since a node with no operator has nothing to report and a line that says nothing reads
/// like a measurement that went missing.
///
/// The condition written twice is the condition that drifts, and the way that failure shows is output
/// claiming the work moved when it did not, or the other way about. So it is written here.
///
/// It does not ask whether the predicate reads as tests, only where the filter sits. A filter over a
/// stored table does the same work in either place, so moving it costs nothing and saves an operator
/// boundary and a chunk handed across it, and the rows the scan produces are already the rows that
/// passed. What the tests decide is the extra thing on top, which is whether a chunk can skip the
/// comparison entirely, and that is what [`Moved::whole`] is for. ClickBench 40 is the case: its
/// `TraficSourceID IN (-1, 6)` becomes an `OR` and an `OR` reads as no test, and requiring every
/// conjunct to read left the whole predicate running above the scan for the sake of a shortcut that
/// would not have fired on it anyway.
///
/// Only [`Node::Get`]. A `read_parquet` wants the same thing and reads its rows through a different
/// loop, so it is its own piece of work rather than a second arm here.
#[must_use]
pub fn into_scan(plan: &Plan, filter: rudb_plan::NodeRef) -> Option<Moved> {
    let Node::Filter { input, predicate } = *plan.node(filter) else { return None };
    if !matches!(*plan.node(input), Node::Get { .. }) {
        return None;
    }
    let index = scanned(plan, input)?;
    let conjuncts = match *plan.expr(predicate) {
        Expr::Conjunction { op: ConjunctionOp::And, children } => plan
            .expr_list(children)
            .iter()
            .map(|&child| {
                let mut tests = Vec::new();
                (every_conjunct(plan, child, index, &mut tests) && !tests.is_empty())
                    .then_some(tests)
            })
            .collect(),
        _ => Vec::new(),
    };
    let mut tests = Vec::new();
    // Thrown away rather than kept when the walk stopped early, because a half read predicate is a
    // list of tests that prove the wrong thing and the only safe use of it is none.
    if !every_conjunct(plan, predicate, index, &mut tests) {
        return Some(Moved { tests: Vec::new(), whole: false, conjuncts });
    }
    Some(Moved { whole: !tests.is_empty(), tests, conjuncts })
}

/// The table index of a scan, or `None` for a node that has no bounds to ask about.
#[must_use]
pub fn scanned(plan: &Plan, node: rudb_plan::NodeRef) -> Option<u32> {
    match *plan.node(node) {
        Node::TableFunction { index, function, .. } => {
            let name = plan.string(function);
            (rudb_functions::TableFunction::lookup(name)
                == Some(rudb_functions::TableFunction::ReadParquet))
            .then_some(index)
        }
        Node::Get { index, .. } => Some(index),
        _ => None,
    }
}

/// Every conjunct of `predicate` that reads as a test on the scan numbered `index`, appended.
fn conjuncts(plan: &Plan, predicate: ExprRef, index: u32, out: &mut Vec<Test>) {
    match *plan.expr(predicate) {
        Expr::Conjunction { op: ConjunctionOp::And, children } => {
            for child in plan.expr_list(children) {
                conjuncts(plan, *child, index, out);
            }
        }
        Expr::Compare { op, left, right } => {
            if let Some(test) = comparison(plan, op, left, right, index) {
                out.push(test);
            }
        }
        _ => {}
    }
}

/// [`conjuncts`] again, answering whether it read all of them rather than only collecting the ones
/// it could.
///
/// Written beside the other walk rather than as one walk with a flag because the two want opposite
/// things from an `OR`. The collecting walk steps over it, since a conjunct under an `OR` says
/// nothing about the row when it is false. This one has to call it unreadable, since a predicate with
/// an `OR` in it is a predicate the tests do not add up to.
fn every_conjunct(plan: &Plan, predicate: ExprRef, index: u32, out: &mut Vec<Test>) -> bool {
    match *plan.expr(predicate) {
        // Stops at the first conjunct it cannot read, which leaves `out` holding the ones before it
        // and is why the caller throws the whole list away rather than using it on a `false`. Half a
        // predicate in there is not a smaller filter, it is the wrong one.
        Expr::Conjunction { op: ConjunctionOp::And, children } => {
            plan.expr_list(children).iter().all(|child| every_conjunct(plan, *child, index, out))
        }
        Expr::Compare { op, left, right } => match comparison(plan, op, left, right, index) {
            Some(test) => {
                out.push(test);
                true
            }
            None => false,
        },
        _ => false,
    }
}

/// One comparison read as a test on a column of the scan numbered `index`, if it is one.
///
/// Written either way round, because `5 < a` and `a > 5` say the same thing and the optimizer does
/// not normalise which side the constant sits on. The comparisons that survive a null are the four
/// orderings and equality: `<>` rules out a stretch only when it holds one distinct value, which no
/// pair of bounds says, and the two distinctness operators are about nulls rather than about order.
fn comparison(
    plan: &Plan,
    op: CompareOp,
    left: ExprRef,
    right: ExprRef,
    index: u32,
) -> Option<Test> {
    let op = match op {
        CompareOp::Equal => Op::Equal,
        CompareOp::Less => Op::Less,
        CompareOp::LessOrEqual => Op::LessOrEqual,
        CompareOp::Greater => Op::Greater,
        CompareOp::GreaterOrEqual => Op::GreaterOrEqual,
        CompareOp::NotEqual | CompareOp::DistinctFrom | CompareOp::NotDistinctFrom => return None,
    };
    let (op, binding, value) = match (plan.expr(left), plan.expr(right)) {
        (Expr::Column(binding), Expr::Constant(value)) => (op, *binding, *value),
        (Expr::Constant(value), Expr::Column(binding)) => (op.flipped(), *binding, *value),
        _ => return None,
    };
    let ColumnBinding { table, column } = binding;
    if table != index {
        return None;
    }
    Some((column as usize, op, Bound::of_value(plan.value(value))?))
}

#[cfg(test)]
mod tests {
    use rudb_common::bounds::{Bound, Op};
    use rudb_plan::Plan;

    use super::of;

    /// The tests a filter over a one column scan reads as, with the filter written as text.
    fn read(predicate: &str) -> Vec<super::Test> {
        let text =
            format!("Filter {predicate}\n  Get memory.main.t AS t #0 [a::INTEGER, b::INTEGER]\n");
        let plan =
            Plan::parse(&text).unwrap_or_else(|error| panic!("{text} did not parse: {error}"));
        let rudb_plan::Node::Filter { input, predicate } = *plan.node(plan.root()) else {
            panic!("the root is the filter");
        };
        of(&plan, input, predicate)
    }

    /// One test, spelled the way the comparisons below come out.
    fn test(column: usize, op: Op, number: i128) -> super::Test {
        (column, op, Bound::Int(number))
    }

    #[test]
    fn a_column_against_a_constant_reads_as_a_test_on_that_column() {
        assert_eq!(read("(#0.0::INTEGER < 5::INTEGER)::BOOLEAN"), vec![test(0, Op::Less, 5)]);
        assert_eq!(read("(#0.1::INTEGER = 9::INTEGER)::BOOLEAN"), vec![test(1, Op::Equal, 9)]);
    }

    #[test]
    fn the_constant_on_the_left_flips_the_comparison_rather_than_reversing_its_meaning() {
        // `5 < a` and `a > 5` are the same filter and nothing normalises which side the constant
        // sits on, so both have to arrive at the same test. Getting this backwards would skip the
        // row groups that hold the answer and keep the ones that do not.
        assert_eq!(read("(5::INTEGER < #0.0::INTEGER)::BOOLEAN"), vec![test(0, Op::Greater, 5)]);
        assert_eq!(
            read("(5::INTEGER >= #0.0::INTEGER)::BOOLEAN"),
            vec![test(0, Op::LessOrEqual, 5)]
        );
        assert_eq!(read("(5::INTEGER = #0.0::INTEGER)::BOOLEAN"), vec![test(0, Op::Equal, 5)]);
    }

    #[test]
    fn every_conjunct_of_an_and_is_read_and_a_disjunction_is_not_walked_into() {
        let and = "((#0.0::INTEGER > 1::INTEGER)::BOOLEAN AND (#0.1::INTEGER < 9::INTEGER)::BOOLEAN)::BOOLEAN";
        assert_eq!(read(and), vec![test(0, Op::Greater, 1), test(1, Op::Less, 9)]);
        // A conjunct under an `OR` says nothing about the row when it is false, so neither branch
        // is a test the whole filter makes and the filter reads as nothing at all.
        let or = "((#0.0::INTEGER > 1::INTEGER)::BOOLEAN OR (#0.1::INTEGER < 9::INTEGER)::BOOLEAN)::BOOLEAN";
        assert_eq!(read(or), Vec::new());
    }

    #[test]
    fn a_conjunct_that_is_not_a_test_is_dropped_and_the_rest_are_still_read() {
        // The conservative direction. A test that is missing costs an estimate above the truth,
        // which is a slower plan, and a test invented out of a comparison of two columns costs
        // rows.
        let mixed = "((#0.0::INTEGER = #0.1::INTEGER)::BOOLEAN AND (#0.1::INTEGER < 9::INTEGER)::BOOLEAN)::BOOLEAN";
        assert_eq!(read(mixed), vec![test(1, Op::Less, 9)]);
        assert_eq!(read("(#0.0::INTEGER <> 5::INTEGER)::BOOLEAN"), Vec::new());
        // A comparison against null is null, so the filter keeps no rows anywhere. That is a fact
        // about the query rather than about one stretch of it and is not decided here.
        assert_eq!(read("(#0.0::INTEGER < NULL::INTEGER)::BOOLEAN"), Vec::new());
    }

    #[test]
    fn a_binding_into_some_other_operator_is_not_a_test_on_this_scan() {
        // Table two is not the table under this filter, so its bounds say nothing about it. The
        // scan is numbered zero and the binding names one, which is a column of a join partner.
        assert_eq!(read("(#1.0::INTEGER < 5::INTEGER)::BOOLEAN"), Vec::new());
    }

    #[test]
    fn a_node_that_is_not_a_scan_has_no_bounds_to_ask_about() {
        let text = "Filter (#0.0::INTEGER < 5::INTEGER)::BOOLEAN\n  Filter (#0.0::INTEGER > 1::INTEGER)::BOOLEAN\n    Get memory.main.t AS t #0 [a::INTEGER]\n";
        let plan = Plan::parse(text).expect("parses");
        let rudb_plan::Node::Filter { input, predicate } = *plan.node(plan.root()) else {
            panic!("the root is the filter");
        };
        assert_eq!(of(&plan, input, predicate), Vec::new());
    }

    /// Each operand of the top level `AND` answers for itself, so the `<>` that stops the whole
    /// predicate reading as tests does not stop the comparison beside it.
    #[test]
    fn each_conjunct_of_a_moved_filter_reads_as_tests_of_its_own() {
        let text = "Filter ((#0.0::INTEGER > 1::INTEGER)::BOOLEAN AND (#0.1::INTEGER <> 9::INTEGER)::BOOLEAN)::BOOLEAN\n  Get memory.main.t AS t #0 [a::INTEGER, b::INTEGER]\n";
        let plan = Plan::parse(text).expect("parses");
        let moved = super::into_scan(&plan, plan.root()).expect("a filter over a stored table");
        assert!(!moved.whole);
        assert_eq!(moved.conjuncts, vec![Some(vec![test(0, Op::Greater, 1)]), None]);
        let one = "Filter (#0.0::INTEGER > 1::INTEGER)::BOOLEAN\n  Get memory.main.t AS t #0 [a::INTEGER]\n";
        let plan = Plan::parse(one).expect("parses");
        let moved = super::into_scan(&plan, plan.root()).expect("a filter over a stored table");
        assert!(moved.whole && moved.conjuncts.is_empty(), "no `AND`, nothing to split");
    }
}
