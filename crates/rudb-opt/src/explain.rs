//! What `EXPLAIN` prints.
//!
//! The logical plan with an estimate beside each operator, the pipeline each operator runs in, the
//! edges between those pipelines, and what is running at every seam. That is what
//! `spec/09-optimizer.md` section 9.5 asks of `EXPLAIN` at this milestone. The physical plan is the
//! other half and it waits for there to be a physical plan: today the executor is built straight
//! off the logical one by `crates/rudb-exec/src/build.rs`, so a physical section would be the same
//! tree with different words on it.
//!
//! `EXPLAIN ANALYZE` is [`analyzed`], which is the same three sections with the numbers of a query
//! that actually ran written next to them. It prints from the metrics document rather than from
//! anything of its own, so what it shows and what `--metrics run.json` writes out are the same
//! numbers read two ways, and a disagreement between a printed plan and a recorded run is not a
//! thing that can happen.
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

use rudb_metrics::{Document, Operator};
use rudb_plan::{Node, NodeRef, OperatorRef, PipelineRef, Plan, Shape};
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

    /// The settings the statement runs under.
    ///
    /// The builder needs them, because an operator that sits on a seam chooses in its constructor,
    /// and `EXPLAIN ANALYZE` runs the query through the same path an ordinary statement takes. This
    /// hands back the same settings this was made from, so the plan that is printed and the tree
    /// that ran chose from the same pins.
    #[must_use]
    pub fn settings(&self) -> &'a Settings {
        self.settings
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
    printed(plan, statistics, seams, None)
}

/// The plan as `EXPLAIN ANALYZE` prints it, which is the same three sections with what happened
/// written next to what was expected.
///
/// The document has to be the one the same plan produced. Every number in the output is looked up
/// by the operator id [`Shape`] gives a node, which is the id the builder tagged that operator's
/// counters with, so a document from a different query lines nothing up rather than lining the
/// wrong things up.
#[must_use]
pub fn analyzed(
    plan: &Plan,
    statistics: &Statistics,
    seams: Seams<'_>,
    measured: &Document,
) -> String {
    printed(plan, statistics, seams, Some(measured))
}

/// Writes the estimated row count of every node onto the operator row that node became.
///
/// Done here because the estimate is here and the mapping from a node to an operator is in
/// `rudb-plan`, and neither of those is something the crate that runs a query should be working out
/// for itself. An estimate an order of magnitude away from what happened is how a bad plan explains
/// itself, and `rudb_metrics::warnings` cannot say so over a document where the estimate is missing.
pub fn record_estimates(plan: &Plan, statistics: &Statistics, document: &mut Document) {
    let shape = Shape::of(plan);
    let mut estimated = vec![None; shape.operators() as usize];
    for node in 0..u32::try_from(plan.node_count()).unwrap_or(u32::MAX) {
        if let Some(id) = shape.operator_of(node) {
            estimated[id as usize] = rows(plan, node, statistics);
        }
    }
    for operator in &mut document.operators {
        if let Some(estimate) = estimated.get(operator.id as usize) {
            operator.estimated_rows = *estimate;
        }
    }
}

/// The three sections, with the measured numbers in them if there are any.
fn printed(
    plan: &Plan,
    statistics: &Statistics,
    seams: Seams<'_>,
    measured: Option<&Document>,
) -> String {
    let shape = Shape::of(plan);
    let printing = Printing { plan, statistics, shape: &shape, seams, measured };
    let mut out = String::new();
    printing.write_node(plan.root(), 0, &mut out);
    write_pipelines(&shape, measured, &mut out);
    write_seams(seams, &mut out);
    if let Some(measured) = measured {
        write_totals(measured, &mut out);
    }
    out
}

/// Everything a line of the tree is written from, which is the same for every line.
///
/// The walk down the tree changes the node and the depth and nothing else, so the rest is carried
/// here rather than as five more arguments repeated at each level.
#[derive(Clone, Copy)]
struct Printing<'a> {
    plan: &'a Plan,
    statistics: &'a Statistics,
    shape: &'a Shape,
    seams: Seams<'a>,
    measured: Option<&'a Document>,
}

impl Printing<'_> {
    fn write_node(self, node: NodeRef, depth: usize, out: &mut String) {
        let printed = self.plan.operator(node);
        let estimate = match rows(self.plan, node, self.statistics) {
            Some(count) => format!("~{count} rows"),
            None => "rows unknown".to_owned(),
        };
        let pipeline = self.shape.pipeline(node);
        let marker =
            if self.seams.all_reference(self.plan.node(node)) { " [reference]" } else { "" };
        let actual = self
            .measured
            .map(|measured| actually(measured, self.shape.operator(node)))
            .unwrap_or_default();
        // The estimate goes after the operator rather than in a column of its own, because the tree
        // is indented and a column would have to be wider than the deepest line to line up.
        let _ = writeln!(
            out,
            "{:indent$}{printed}  [{estimate}] [pipeline {pipeline}]{marker}{actual}",
            "",
            indent = depth * 2
        );
        if let (Some(measured), Some(gathered)) = (self.measured, self.shape.gathered(node)) {
            // The operator holding the side that finishes first has no line of the plan to sit on,
            // because it is not a node. It gets its own line under the one it belongs to rather
            // than being left out, since it is where the time of a build side actually goes.
            if let Some(operator) = row(measured, gathered) {
                let _ = writeln!(
                    out,
                    "{:indent$}{} of the side that finishes first{}",
                    "",
                    operator.kind,
                    actually(measured, gathered),
                    indent = (depth + 1) * 2
                );
            }
        }
        for child in children(self.plan.node(node)) {
            self.write_node(child, depth + 1, out);
        }
    }
}

/// What one operator did, as it goes on the end of its line.
fn actually(measured: &Document, id: OperatorRef) -> String {
    let Some(operator) = row(measured, id) else {
        return "  [not measured]".to_owned();
    };
    let held = operator.memory.high_water;
    let memory = if held == 0 { String::new() } else { format!(", {} held", bytes(held)) };
    // The fall backs are on the operator line rather than only in the totals, because the number
    // is only worth having if a reader can see which node it belongs to without counting rows.
    let slow = match operator.fallbacks.worst() {
        None => String::new(),
        Some((cause, _)) => {
            format!(", {} fell back, most of it {}", operator.fallbacks.total(), cause.name())
        }
    };
    format!("  [{} rows, {}{memory}{slow}]", operator.rows_out, duration(operator.wall_ns))
}

/// The operator row with this id.
fn row(measured: &Document, id: OperatorRef) -> Option<&Operator> {
    measured.operators.iter().find(|operator| operator.id == id)
}

/// The pipelines and what each of them waits for.
///
/// Printed even when there is only one, because a reader who sees no section cannot tell a plan
/// that does not break from a build of `EXPLAIN` that does not say.
fn write_pipelines(shape: &Shape, measured: Option<&Document>, out: &mut String) {
    let _ = writeln!(out, "\nPipelines");
    for pipeline in shape.all() {
        let waits = shape.waits_for(pipeline);
        let waiting = if waits.is_empty() {
            "waits for nothing".to_owned()
        } else {
            format!("waits for {}", listed(waits))
        };
        let root = if pipeline == ROOT { ", and the answer comes out of it" } else { "" };
        let took = measured
            .and_then(|measured| measured.pipelines.iter().find(|row| row.id == pipeline))
            .map(|row| format!("  [{} wall, {} cpu]", duration(row.wall_ns), duration(row.cpu_ns)))
            .unwrap_or_default();
        let _ = writeln!(out, "  pipeline {pipeline} {waiting}{root}{took}");
    }
}

/// The whole query's numbers, and anything the document has to warn about them.
fn write_totals(measured: &Document, out: &mut String) {
    let timing = &measured.timing;
    let _ = writeln!(out, "\nTotals");
    let _ = writeln!(
        out,
        "  {} building the tree, {} running it, {} in all",
        duration(timing.physical_ns),
        duration(timing.execute_ns),
        duration(timing.total_ns)
    );
    let _ = writeln!(
        out,
        "  {} of cpu, {} held at the peak",
        duration(measured.resource.cpu_ns),
        bytes(measured.resource.peak_bytes)
    );
    let warnings = measured.warnings();
    if !warnings.is_empty() {
        let _ = writeln!(out, "\nWarnings");
        for warning in &warnings {
            let _ = writeln!(out, "  {warning}");
        }
    }
}

/// A duration, in whichever unit a person would say it in.
///
/// Three significant figures and no more, because the fourth is noise on any measurement this is
/// printing and a reader who sees it starts believing it.
fn duration(ns: u64) -> String {
    match ns {
        0 => "0s".to_owned(),
        1..1_000 => format!("{ns}ns"),
        1_000..1_000_000 => format!("{:.3}us", ns as f64 / 1_000.0),
        1_000_000..1_000_000_000 => format!("{:.3}ms", ns as f64 / 1_000_000.0),
        _ => format!("{:.3}s", ns as f64 / 1_000_000_000.0),
    }
}

/// A byte count, in whichever unit a person would say it in.
fn bytes(count: u64) -> String {
    match count {
        0..1024 => format!("{count} bytes"),
        1024..1_048_576 => format!("{:.1} KiB", count as f64 / 1024.0),
        1_048_576..1_073_741_824 => format!("{:.1} MiB", count as f64 / 1_048_576.0),
        _ => format!("{:.1} GiB", count as f64 / 1_073_741_824.0),
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
        Node::Filter { .. } => &[
            SeamId::ExprEval,
            SeamId::KernelCompare,
            SeamId::KernelFilter,
            SeamId::ChunkCompaction,
        ],
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
