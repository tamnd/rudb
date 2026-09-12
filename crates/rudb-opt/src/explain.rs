//! What `EXPLAIN` prints.
//!
//! The logical plan with an estimate beside each operator, the pipeline each operator runs in, the
//! edges between those pipelines, and what is running at every seam. That is what
//! `spec/09-optimizer.md` section 9.5 asks of `EXPLAIN` at this milestone. The physical plan is the
//! other half and it waits for there to be a physical plan: today the executor is built straight
//! off the logical one by `crates/rudb-exec/src/build.rs`, so a physical section would be the same
//! tree with different words on it. `EXPLAIN ANALYZE` is the third and it needs the metrics
//! document of a query that actually ran.
//!
//! This is in the optimizer rather than in `rudb-plan` because the estimate is here, and printing
//! the plan without the estimate would be `Plan::to_string`, which already exists.
//!
//! # Why the pipelines are not worked out here
//!
//! They come from [`Shape`], which lives in `rudb-plan`, and the executor builds its tree out of the
//! same call. Two walks that both decide where a plan breaks would agree on the day they were
//! written and disagree some time after, and the one that would be wrong is this one, which is the
//! one somebody reads when they are trying to find out why a query is slow.
//!
//! # What the reference marker means
//!
//! Every operator at F0 is running the simplest correct implementation of everything it does, and a
//! number measured against a reference implementation is not a number worth quoting as the engine's.
//! So every line says so, and the marker comes off a line on the day one of the seams under it has
//! a second implementation registered and chosen. It is read off the registries rather than written
//! down here, so nobody has to remember to take it off.
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

use rudb_plan::{Node, NodeRef, PipelineRef, Plan, Shape};
use rudb_seam::{Registries, SeamId, Settings};

use crate::estimate::{Statistics, rows};

/// What `EXPLAIN` needs to know about the seams to print the last section.
///
/// Two borrows rather than one, because what is registered is a property of the process and what is
/// pinned is a property of the session, and a query that was run with a hint on it prints
/// differently from the same query without one.
#[derive(Debug, Clone, Copy)]
pub struct Seams<'a> {
    settings: &'a Settings,
    registries: &'a Registries,
}

impl<'a> Seams<'a> {
    /// The settings a statement ran under, against the registries the process assembled.
    #[must_use]
    pub fn new(settings: &'a Settings, registries: &'a Registries) -> Self {
        Self { settings, registries }
    }

    /// The name of what runs at this seam, and whether that thing is the reference.
    ///
    /// `None` when the seam has no registry, which at F0 is all twenty seven of them. That is not
    /// the same as nothing running: the reference implementation is compiled in and is what the
    /// operators call. It means there is nothing to choose between and therefore nothing to print.
    fn chosen(self, seam: SeamId) -> Option<(String, bool)> {
        if !self.registries.has(seam) {
            return None;
        }
        let rows = self.registries.rows();
        let rows = rows.iter().filter(|row| row.seam == seam);
        if let Some(pinned) = self.settings.pinned(seam) {
            let is_reference =
                rows.clone().find(|row| row.name == pinned).is_some_and(|row| row.is_reference);
            return Some((format!("{pinned} (pinned)"), is_reference));
        }
        let row = rows.clone().find(|row| row.is_default).or_else(|| rows.clone().next())?;
        Some((format!("{} (default)", row.name), row.is_reference))
    }

    /// Whether everything this node does is a reference implementation.
    fn all_reference(self, node: &Node) -> bool {
        seams_of(node).iter().all(|seam| self.chosen(*seam).is_none_or(|(_, reference)| reference))
    }
}

/// The plan as `EXPLAIN` prints it, for a caller with no seam state to hand.
///
/// The plan section is the same either way. What is missing is the seam section, which is why this
/// exists for tests and for anything that wants the tree and nothing else.
#[must_use]
pub fn explain(plan: &Plan, statistics: &Statistics) -> String {
    let settings = Settings::new();
    let registries = Registries::new();
    explain_with(plan, statistics, Seams::new(&settings, &registries))
}

/// The plan as `EXPLAIN` prints it: the tree, then the pipelines, then the seams.
///
/// Each line of the tree is the operator as [`Plan::operator`] writes it with the estimate, the
/// pipeline and the reference marker appended, so the arguments that decide what an operator does
/// are all still there and a reader who knows the plan format already knows this one.
///
/// An operator whose estimate is unknown says so rather than being left blank. A blank reads as
/// zero, and the difference between "no rows" and "nobody knows" is the whole of what
/// [`crate::estimate`] is careful about.
#[must_use]
pub fn explain_with(plan: &Plan, statistics: &Statistics, seams: Seams<'_>) -> String {
    let shape = Shape::of(plan);
    let mut out = String::new();
    write_node(plan, statistics, &shape, seams, plan.root(), 0, &mut out);
    write_pipelines(&shape, &mut out);
    write_seams(seams, &mut out);
    out
}

fn write_node(
    plan: &Plan,
    statistics: &Statistics,
    shape: &Shape,
    seams: Seams<'_>,
    node: NodeRef,
    depth: usize,
    out: &mut String,
) {
    let printed = plan.operator(node);
    let estimate = match rows(plan, node, statistics) {
        Some(count) => format!("~{count} rows"),
        None => "rows unknown".to_owned(),
    };
    let pipeline = shape.pipeline(node);
    let marker = if seams.all_reference(plan.node(node)) { " [reference]" } else { "" };
    // The estimate goes after the operator rather than in a column of its own, because the tree is
    // indented and a column would have to be wider than the deepest line to line up.
    let _ = writeln!(
        out,
        "{:indent$}{printed}  [{estimate}] [pipeline {pipeline}]{marker}",
        "",
        indent = depth * 2
    );
    for child in children(plan.node(node)) {
        write_node(plan, statistics, shape, seams, child, depth + 1, out);
    }
}

/// The pipelines and what each of them waits for.
///
/// Printed even when there is only one, because a reader who sees no section cannot tell a plan
/// that does not break from a build of `EXPLAIN` that does not say.
fn write_pipelines(shape: &Shape, out: &mut String) {
    let _ = writeln!(out, "\nPipelines");
    for pipeline in shape.all() {
        let waits = shape.waits_for(pipeline);
        let waiting = if waits.is_empty() {
            "waits for nothing".to_owned()
        } else {
            format!("waits for {}", listed(waits))
        };
        let root = if pipeline == ROOT { ", and the answer comes out of it" } else { "" };
        let _ = writeln!(out, "  pipeline {pipeline} {waiting}{root}");
    }
}

/// What is running at every seam that has something to choose between.
///
/// A seam with no registry is not listed one line at a time. There are twenty seven of them and
/// listing every one on every `EXPLAIN` would bury the plan under a table that says the same thing
/// every time, so the count is given and `rudb_strategies()` is where the list is.
fn write_seams(seams: Seams<'_>, out: &mut String) {
    let _ = writeln!(out, "\nSeams");
    let mut printed = 0;
    for seam in SeamId::ALL {
        if let Some((chosen, _)) = seams.chosen(*seam) {
            let _ = writeln!(out, "  {} = {chosen}", seam.name());
            printed += 1;
        }
    }
    let unregistered = SeamId::ALL.len() - printed;
    if unregistered > 0 {
        let _ = writeln!(
            out,
            "  {unregistered} seams have nothing registered and are running their reference implementation, see rudb_strategies()"
        );
    }
}

/// The pipeline the answer comes out of.
const ROOT: PipelineRef = 0;

/// A list of pipeline numbers, as somebody would say it out loud.
fn listed(pipelines: &[PipelineRef]) -> String {
    let numbers: Vec<String> = pipelines.iter().map(u32::to_string).collect();
    match numbers.split_last() {
        None => String::new(),
        Some((last, [])) => last.clone(),
        Some((last, rest)) => format!("{} and {last}", rest.join(", ")),
    }
}

/// The children of a node, in the order they print.
fn children(node: &Node) -> Vec<NodeRef> {
    node.children().into_iter().flatten().collect()
}

/// The seams an operator's answer depends on.
///
/// This is the list that decides whether a line gets the reference marker, so it is the operator's
/// own seams rather than every seam a query touches. A scan sits on how a column is carried and on
/// when a column is read, a join sits on how its build side is made probeable and on the three hash
/// seams under that, and a limit sits on nothing at all because there is one way to count to ten.
fn seams_of(node: &Node) -> &'static [SeamId] {
    const HASHED: &[SeamId] = &[SeamId::HashKey, SeamId::HashFunction, SeamId::HashTable];
    match node {
        Node::Get { .. } => &[SeamId::VectorForm, SeamId::ScanMaterialisation],
        Node::Filter { .. } => &[SeamId::ExprEval, SeamId::KernelCompare, SeamId::KernelFilter],
        Node::Project { .. } => &[SeamId::ExprEval],
        Node::Aggregate { .. } => &[
            SeamId::HashKey,
            SeamId::HashFunction,
            SeamId::HashTable,
            SeamId::AggState,
            SeamId::AggParallel,
        ],
        Node::Distinct { .. } | Node::SetOp { .. } => HASHED,
        Node::Sort { .. } => &[SeamId::Sort],
        Node::TopN { .. } => &[SeamId::TopK],
        Node::Join { .. } => {
            &[SeamId::JoinBuild, SeamId::JoinFilter, SeamId::HashKey, SeamId::HashFunction]
        }
        Node::Dummy
        | Node::Values { .. }
        | Node::TableFunction { .. }
        | Node::Limit { .. }
        | Node::CrossProduct { .. } => &[],
    }
}

#[cfg(test)]
mod tests {
    use rudb_plan::Plan;
    use rudb_seam::{Registries, Settings};

    use super::{Seams, explain, explain_with};
    use crate::estimate::Statistics;

    fn parsed(text: &str) -> Plan {
        Plan::parse(text).unwrap_or_else(|error| panic!("{text} did not parse: {error}"))
    }

    fn printed(text: &str, tables: &[(&str, u64)]) -> String {
        let mut statistics = Statistics::new();
        for (table, count) in tables {
            statistics.record("memory", "main", table, *count);
        }
        explain(&parsed(text), &statistics)
    }

    /// The tree, without the sections under it, which is the part most tests are about.
    fn tree(out: &str) -> Vec<&str> {
        out.lines().take_while(|line| !line.is_empty()).collect()
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
            tree(&out).join("\n"),
            concat!(
                "Filter (#0.0::INTEGER > 1::INTEGER)::BOOLEAN  [~200 rows] [pipeline 0] [reference]\n",
                "  Get memory.main.t AS t #0 [a::INTEGER]  [~1000 rows] [pipeline 0] [reference]",
            )
        );
    }

    #[test]
    fn an_operator_nobody_can_estimate_says_so_rather_than_saying_nothing() {
        // Blank would read as zero, and a reader who takes an unknown for an empty relation is the
        // reader this wording exists for.
        let out = printed("Get memory.main.t AS t #0 [a::INTEGER]\n", &[]);
        assert!(tree(&out)[0].contains("[rows unknown]"), "{out}");
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
        let lines = tree(&out);
        assert_eq!(lines.len(), 3, "{out}");
        assert!(lines[0].contains("[~5000 rows]"), "{out}");
        assert!(lines[1].contains("small") && lines[1].contains("[~10 rows]"), "{out}");
        assert!(lines[2].contains("big") && lines[2].contains("[~5000 rows]"), "{out}");
        // Indented by depth, so the shape of the tree survives being flattened into lines.
        assert!(lines[1].starts_with("  Get"), "{out}");
    }

    #[test]
    fn a_plan_that_does_not_break_is_one_pipeline_and_says_so() {
        let out = printed("Get memory.main.t AS t #0 [a::INTEGER]\n", &[("t", 4)]);
        assert!(out.contains("[pipeline 0]"), "{out}");
        assert!(
            out.contains("  pipeline 0 waits for nothing, and the answer comes out of it"),
            "{out}"
        );
    }

    #[test]
    fn a_sort_prints_the_pipeline_it_ends_and_the_edge_above_it() {
        let out = printed(
            concat!(
                "Sort [#0.0::INTEGER ASC NULLS LAST]\n",
                "  Get memory.main.t AS t #0 [a::INTEGER]\n",
            ),
            &[("t", 100)],
        );
        let lines = tree(&out);
        assert!(lines[0].contains("[pipeline 1]"), "the sort ends the one below it: {out}");
        assert!(lines[1].contains("[pipeline 1]"), "{out}");
        assert!(out.contains("  pipeline 0 waits for 1"), "{out}");
        assert!(out.contains("  pipeline 1 waits for nothing"), "{out}");
    }

    #[test]
    fn a_join_prints_three_pipelines_in_the_order_they_have_to_run() {
        let out = printed(
            concat!(
                "Join INNER on=[(#0.0::INTEGER = #1.0::INTEGER)::BOOLEAN]\n",
                "  Get memory.main.l AS l #0 [a::INTEGER]\n",
                "  Get memory.main.r AS r #1 [a::INTEGER]\n",
            ),
            &[("l", 10), ("r", 10)],
        );
        let lines = tree(&out);
        assert!(lines[0].contains("[pipeline 2]"), "{out}");
        assert!(lines[1].contains("[pipeline 2]"), "the probing side: {out}");
        assert!(lines[2].contains("[pipeline 1]"), "the gathered side runs first: {out}");
        assert!(out.contains("  pipeline 0 waits for 2"), "{out}");
        assert!(out.contains("  pipeline 2 waits for 1"), "{out}");
    }

    #[test]
    fn with_nothing_registered_the_seam_section_says_what_that_means() {
        let out = printed("Get memory.main.t AS t #0 [a::INTEGER]\n", &[("t", 4)]);
        assert!(out.contains("\nSeams\n"), "{out}");
        assert!(
            out.contains(
                "  27 seams have nothing registered and are running their reference implementation"
            ),
            "{out}"
        );
    }

    #[test]
    fn a_hint_that_pins_a_seam_nobody_has_registered_changes_nothing_that_prints() {
        // The settings are carried all the way down here, so the section has to be the registries
        // and the settings together rather than either one alone. Today the registries are empty,
        // so a pin has nothing to pin and the print says the same thing.
        let mut settings = Settings::new();
        settings.pin(rudb_seam::SeamId::Sort, "merge");
        let registries = Registries::new();
        let plan = parsed(
            "Sort [#0.0::INTEGER ASC NULLS LAST]\n  Get memory.main.t AS t #0 [a::INTEGER]\n",
        );
        let out = explain_with(&plan, &Statistics::new(), Seams::new(&settings, &registries));
        assert!(out.contains("[reference]"), "{out}");
        assert!(!out.contains("sort = "), "{out}");
    }
}
