//! What `EXPLAIN` prints.
//!
//! The logical plan with an estimate beside each operator, which is what `spec/09-optimizer.md`
//! section 9.5 asks of `EXPLAIN` at this milestone. The physical plan is the other half and it
//! waits for there to be a physical plan: today the executor is built straight off the logical one
//! by `crates/rudb-exec/src/build.rs`, so a physical section would be the same tree with different
//! words on it. `EXPLAIN ANALYZE` is the third and it needs per operator instrumentation over a
//! query that actually ran.
//!
//! This is in the optimizer rather than in `rudb-plan` because the estimate is here, and printing
//! the plan without the estimate would be `Plan::to_string`, which already exists.
//!
//! # This output is not a compatibility surface
//!
//! `spec/12-duckdb-compat.md` section 12.5 excludes `EXPLAIN` text from the guarantee, and section
//! 9.5 says why: matching DuckDB's explain text would pin our optimizer to their operator
//! vocabulary, and their vocabulary is a physical one with a `HASH_JOIN` in it. So the shape is
//! DuckDB's, two `VARCHAR` columns called `explain_key` and `explain_value`, because that is what a
//! client reading a result set has to cope with, and the text inside the second column is ours.
//!
//! It is stable enough to commit as a test baseline within a minor version, which is what section
//! 11.8's plan stability test is going to read.

use std::fmt::Write as _;

use rudb_plan::{Node, NodeRef, Plan};

use crate::estimate::{Statistics, rows};

/// The plan as `EXPLAIN` prints it, one line per operator, indented by depth.
///
/// Each line is the operator as [`Plan::operator`] writes it with the estimate appended, so the
/// arguments that decide what an operator does are all still there and a reader who knows the plan
/// format already knows this one.
///
/// An operator whose estimate is unknown says so rather than being left blank. A blank reads as
/// zero, and the difference between "no rows" and "nobody knows" is the whole of what
/// [`crate::estimate`] is careful about.
#[must_use]
pub fn explain(plan: &Plan, statistics: &Statistics) -> String {
    let mut out = String::new();
    write_node(plan, statistics, plan.root(), 0, &mut out);
    out
}

fn write_node(plan: &Plan, statistics: &Statistics, node: NodeRef, depth: usize, out: &mut String) {
    let printed = plan.operator(node);
    let estimate = match rows(plan, node, statistics) {
        Some(count) => format!("~{count} rows"),
        None => "rows unknown".to_owned(),
    };
    // The estimate goes after the operator rather than in a column of its own, because the tree is
    // indented and a column would have to be wider than the deepest line to line up.
    let _ = writeln!(out, "{:indent$}{printed}  [{estimate}]", "", indent = depth * 2);
    for child in children(plan.node(node)) {
        write_node(plan, statistics, child, depth + 1, out);
    }
}

/// The children of a node, in the order they print.
fn children(node: &Node) -> Vec<NodeRef> {
    node.children().into_iter().flatten().collect()
}

#[cfg(test)]
mod tests {
    use rudb_plan::Plan;

    use super::explain;
    use crate::estimate::Statistics;

    fn printed(text: &str, tables: &[(&str, u64)]) -> String {
        let mut statistics = Statistics::new();
        for (table, count) in tables {
            statistics.record("memory", "main", table, *count);
        }
        let plan =
            Plan::parse(text).unwrap_or_else(|error| panic!("{text} did not parse: {error}"));
        explain(&plan, &statistics)
    }

    #[test]
    fn every_operator_gets_a_line_with_its_own_estimate_on_it() {
        let out = printed(
            concat!(
                "Filter (#0.0::INTEGER > 1::INTEGER)::BOOLEAN\n",
                "  Get memory.main.t AS t #0 [a::INTEGER]\n",
            ),
            &[("t", 1000)],
        );
        assert_eq!(
            out,
            concat!(
                "Filter (#0.0::INTEGER > 1::INTEGER)::BOOLEAN  [~200 rows]\n",
                "  Get memory.main.t AS t #0 [a::INTEGER]  [~1000 rows]\n",
            )
        );
    }

    #[test]
    fn an_operator_nobody_can_estimate_says_so_rather_than_saying_nothing() {
        // Blank would read as zero, and a reader who takes an unknown for an empty relation is the
        // reader this wording exists for.
        let out = printed("Get memory.main.t AS t #0 [a::INTEGER]\n", &[]);
        assert_eq!(out, "Get memory.main.t AS t #0 [a::INTEGER]  [rows unknown]\n");
    }

    #[test]
    fn both_sides_of_a_join_are_printed_under_it_and_each_carries_its_own_number() {
        let out = printed(
            concat!(
                "Join INNER on=[(#0.0::INTEGER = #1.0::INTEGER)::BOOLEAN]\n",
                "  Get memory.main.small AS small #0 [a::INTEGER]\n",
                "  Get memory.main.big AS big #1 [a::INTEGER]\n",
            ),
            &[("small", 10), ("big", 5000)],
        );
        let lines: Vec<&str> = out.lines().collect();
        assert_eq!(lines.len(), 3, "{out}");
        assert!(lines[0].ends_with("[~5000 rows]"), "{out}");
        assert!(lines[1].contains("small") && lines[1].ends_with("[~10 rows]"), "{out}");
        assert!(lines[2].contains("big") && lines[2].ends_with("[~5000 rows]"), "{out}");
        // Indented by depth, so the shape of the tree survives being flattened into lines.
        assert!(lines[1].starts_with("  Get"), "{out}");
    }
}
