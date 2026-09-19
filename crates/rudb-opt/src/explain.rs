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

use rudb_common::stat::{Class, Classes, Stat, Use};
use rudb_metrics::{Document, Operator};
use rudb_plan::{Node, NodeRef, OperatorRef, PipelineRef, Plan, Shape, seams_of};
use rudb_seam::{Registries, SeamId, Settings};

use crate::estimate::{CARDINALITY, Facts, rows_stat};

/// Whether `EXPLAIN` was asked what the planner knew.
///
/// A named pair rather than a `bool`, because `explain_with(plan, facts, seams, true)` at a call
/// site says nothing about what is true.
///
/// It is off by default because the plan is what somebody reading `EXPLAIN` came for, and a use and
/// a class on every line is a second sentence per line for a question most readers are not asking.
/// `EXPLAIN (STATISTICS)` is the question, and `spec/stats/05-every-query.md` section 5.1.1 is what
/// it answers.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Statistics {
    /// Say what each estimate was read for, and count the classes underneath.
    Asked,
    /// The plan, the pipelines and the seams, which is what a plain `EXPLAIN` prints.
    NotAsked,
}

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

    /// The name of what runs at this seam, said the way the seam section prints it.
    ///
    /// `None` when the seam has no registry, which is what [`Registries::running`] means by it and
    /// is the same answer the executor gets. The formatting is this section's, the decision is not.
    fn chosen(self, seam: SeamId) -> Option<(String, bool)> {
        let running = self.registries.running(seam, self.settings)?;
        let how = if running.pinned { "pinned" } else { "default" };
        Some((format!("{} ({how})", running.name), running.is_reference))
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
pub fn explain(plan: &Plan, facts: &Facts) -> String {
    let settings = Settings::new();
    let registries = Registries::new();
    explain_with(plan, facts, Seams::new(&settings, &registries), Statistics::NotAsked)
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
pub fn explain_with(
    plan: &Plan,
    facts: &Facts,
    seams: Seams<'_>,
    statistics: Statistics,
) -> String {
    printed(plan, facts, seams, None, statistics)
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
    facts: &Facts,
    seams: Seams<'_>,
    measured: &Document,
    statistics: Statistics,
) -> String {
    printed(plan, facts, seams, Some(measured), statistics)
}

/// Writes the estimated row count of every node onto the operator row that node became, and the
/// histogram of how much the planner knew onto the document.
///
/// Done here because the estimate is here and the mapping from a node to an operator is in
/// `rudb-plan`, and neither of those is something the crate that runs a query should be working out
/// for itself. An estimate an order of magnitude away from what happened is how a bad plan explains
/// itself, and `rudb_metrics::warnings` cannot say so over a document where the estimate is missing.
///
/// The histogram counts one decision per operator and not per node, for the same reason the row
/// counts go on operators: a node the physical plan folded away is not a decision anybody acted on,
/// and counting it would move the number with a plan rewrite that changed nothing about what was
/// known.
pub fn record_estimates(plan: &Plan, facts: &Facts, document: &mut Document) {
    let shape = Shape::of(plan);
    let mut estimated = vec![Stat::Unknown; shape.operators() as usize];
    for node in 0..u32::try_from(plan.node_count()).unwrap_or(u32::MAX) {
        if let Some(id) = shape.operator_of(node) {
            estimated[id as usize] = rows_stat(plan, node, facts);
        }
    }
    for operator in &mut document.operators {
        if let Some(estimate) = estimated.get(operator.id as usize) {
            operator.estimated_rows = estimate.value().copied();
            operator.estimate_class = estimate.class();
            operator.estimate_provenance = estimate.provenance();
            document.estimates.record(estimate);
        }
    }
}

/// The three sections, with the measured numbers in them if there are any.
fn printed(
    plan: &Plan,
    facts: &Facts,
    seams: Seams<'_>,
    measured: Option<&Document>,
    statistics: Statistics,
) -> String {
    let shape = Shape::of(plan);
    let printing = Printing { plan, facts, shape: &shape, seams, measured, statistics };
    let mut out = String::new();
    printing.write_node(plan.root(), 0, &mut out);
    write_pipelines(&shape, measured, &mut out);
    write_seams(seams, &mut out);
    if statistics == Statistics::Asked {
        write_statistics(reads(plan, facts, &shape), &mut out);
    }
    if let Some(measured) = measured {
        write_totals(measured, &mut out);
    }
    out
}

/// The row count and its class, as the bracket on a plan line reads.
///
/// The tilde is on the guesses and nothing else. A count that was counted is printed without one
/// because it is not approximately anything, and the words after the number say what kind of
/// knowledge it is and where it came from, which is the question somebody asks when a plan went
/// wrong. `spec/stats/04-in-memory.md` section 4.1 asks for both to be printed for exactly that
/// reason: a bad plan is diagnosed by asking which number was wrong and who produced it, and
/// `estimated from default` is the answer that says nobody had a number here at all.
///
/// The provenance is printed next to an exact number too, per `spec/stats/02-the-catalogue.md`
/// section 2.1.1. An exact count out of the catalog and an exact join cardinality out of a link
/// header are different kinds of exact and a reader has to be able to tell them apart.
/// With the statistics asked for, the use the number was read for goes on the end of the same
/// bracket. It belongs next to the class and not in a section of its own, because the question it
/// answers is about this line: a guess read to decide is a slow query at worst, and the same guess
/// read to enable would be a wrong answer, so the pair is what says whether a line is safe.
fn estimate(stat: Stat<u64>, statistics: Statistics) -> String {
    let read = match statistics {
        Statistics::Asked => format!(", read to {CARDINALITY}"),
        Statistics::NotAsked => String::new(),
    };
    match stat {
        Stat::Unknown => format!("rows unknown{read}"),
        Stat::Known { value, class, provenance } => match class {
            Class::Estimated => format!("~{value} rows {class} from {provenance}{read}"),
            class => format!("{value} rows {class} from {provenance}{read}"),
        },
    }
}

/// Everything a line of the tree is written from, which is the same for every line.
///
/// The walk down the tree changes the node and the depth and nothing else, so the rest is carried
/// here rather than as five more arguments repeated at each level.
#[derive(Clone, Copy)]
struct Printing<'a> {
    plan: &'a Plan,
    facts: &'a Facts,
    shape: &'a Shape,
    seams: Seams<'a>,
    measured: Option<&'a Document>,
    statistics: Statistics,
}

impl Printing<'_> {
    fn write_node(self, node: NodeRef, depth: usize, out: &mut String) {
        let printed = self.plan.operator(node);
        let estimate = estimate(rows_stat(self.plan, node, self.facts), self.statistics);
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

/// How many numbers were read for each of the three uses, and what class each read got.
///
/// Three histograms rather than one, because the class that matters depends on the use. Half the
/// decisions being guesses is a planner with thin statistics and a slow query at the end of it,
/// and one enable on a guess would be a bug the class rule is there to make impossible. A single
/// count could not tell those apart.
#[derive(Debug, Clone, Copy, Default)]
struct Reads {
    answer: Classes,
    enable: Classes,
    decide: Classes,
}

impl Reads {
    /// Counts one read made for that use.
    fn record(&mut self, use_: Use, stat: &Stat<u64>) {
        match use_ {
            Use::Answer => self.answer.record(stat),
            Use::Enable => self.enable.record(stat),
            Use::Decide => self.decide.record(stat),
        }
    }

    /// The histogram for one use.
    const fn of(self, use_: Use) -> Classes {
        match use_ {
            Use::Answer => self.answer,
            Use::Enable => self.enable,
            Use::Decide => self.decide,
        }
    }
}

/// Every statistic this plan was built out of, counted by what it was read for.
///
/// One read per operator and not per node, the same rule [`record_estimates`] counts by and for the
/// same reason: a node the plan folded away is not a decision anybody acted on.
fn reads(plan: &Plan, facts: &Facts, shape: &Shape) -> Reads {
    let mut reads = Reads::default();
    for node in 0..u32::try_from(plan.node_count()).unwrap_or(u32::MAX) {
        if shape.operator_of(node).is_some() {
            reads.record(CARDINALITY, &rows_stat(plan, node, facts));
        }
    }
    reads
}

/// What the planner knew, which is the section `EXPLAIN (STATISTICS)` is asked for.
///
/// A line per use that happened and one line for the uses that did not, rather than three lines of
/// zeroes. The uses that did not happen are named instead of being left out, because the line
/// saying nothing was read to enable is the reassuring half of this section and a reader cannot get
/// it from an absence.
fn write_statistics(reads: Reads, out: &mut String) {
    let _ = writeln!(out, "\nStatistics");
    let mut silent = Vec::new();
    for use_ in [Use::Answer, Use::Enable, Use::Decide] {
        let classes = reads.of(use_);
        if classes.total() == 0 {
            silent.push(format!("to {}", use_.name()));
            continue;
        }
        let share = classes.known_share() * 100.0;
        let _ = writeln!(
            out,
            "  {} read to {use_}: {classes}, {share:.0}% of them with a number behind them",
            classes.total()
        );
    }
    if !silent.is_empty() {
        let _ = writeln!(out, "  nothing was read {}", among(&silent, "or"));
    }
}

/// The whole query's numbers, and anything the document has to warn about them.
fn write_totals(measured: &Document, out: &mut String) {
    let timing = &measured.timing;
    let _ = writeln!(out, "\nTotals");
    // Planning gets its own figure rather than being folded into the build, because the build is
    // one walk over a finished plan and the planning is every pass that decided what the plan was.
    // A query whose optimizer costs more than its execution is a query the optimizer made worse,
    // and this line is where that is visible without anybody going looking for it.
    let planning =
        timing.parse_ns.saturating_add(timing.bind_ns).saturating_add(timing.optimize_ns);
    let _ = writeln!(
        out,
        "  {} planning, {} building the tree, {} running it, {} in all",
        duration(planning),
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
    among(&pipelines.iter().map(u32::to_string).collect::<Vec<String>>(), "and")
}

/// A list of anything, as somebody would say it out loud.
///
/// The conjunction is given rather than always being `and`, because a list of things that did not
/// happen reads as `or` and a reader who is told two uses happened when neither did has been told
/// the opposite of the truth.
fn among(words: &[String], conjunction: &str) -> String {
    match words.split_last() {
        None => String::new(),
        Some((last, [])) => last.clone(),
        Some((last, rest)) => format!("{} {conjunction} {last}", rest.join(", ")),
    }
}

/// The children of a node, in the order they print.
fn children(node: &Node) -> Vec<NodeRef> {
    node.children().into_iter().flatten().collect()
}

#[cfg(test)]
mod tests {
    use rudb_metrics::{Document, Operator};
    use rudb_plan::Plan;
    use rudb_seam::{Registries, Settings};

    use super::{Seams, Shape, Statistics, explain, explain_with, record_estimates};
    use crate::estimate::Facts;

    fn parsed(text: &str) -> Plan {
        Plan::parse(text).unwrap_or_else(|error| panic!("{text} did not parse: {error}"))
    }

    fn printed(text: &str, tables: &[(&str, u64)]) -> String {
        let mut facts = Facts::new();
        for (table, count) in tables {
            facts.record("memory", "main", table, *count);
        }
        explain(&parsed(text), &facts)
    }

    /// The same thing with the statistics asked for, which is `EXPLAIN (STATISTICS)`.
    fn asked(text: &str, tables: &[(&str, u64)]) -> String {
        let mut facts = Facts::new();
        for (table, count) in tables {
            facts.record("memory", "main", table, *count);
        }
        let settings = Settings::new();
        let registries = Registries::new();
        explain_with(&parsed(text), &facts, Seams::new(&settings, &registries), Statistics::Asked)
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
        // The scan says exact because the catalog counted those rows, and the filter above it says
        // estimated because the fifth it took off them is a constant somebody picked. A reader
        // deciding whether to trust the number wants to be told which of the two it is.
        assert_eq!(
            tree(&out).join("\n"),
            concat!(
                "Filter (#0.0::INTEGER > 1::INTEGER)::BOOLEAN  [~200 rows estimated from default] \
                 [pipeline 0] [reference]\n",
                "  Get memory.main.t AS t #0 [a::INTEGER]  [1000 rows exact from row count] [pipeline 0] \
                 [reference]",
            )
        );
    }

    #[test]
    fn a_plain_explain_says_nothing_about_uses_and_asking_for_the_statistics_says_it_on_every_line()
    {
        let plan = concat!(
            "Filter (#0.0::INTEGER > 1::INTEGER)::BOOLEAN\n",
            "  Get memory.main.t AS t #0 [a::INTEGER]\n",
        );
        let quiet = printed(plan, &[("t", 1000)]);
        assert!(!quiet.contains("read to"), "{quiet}");
        assert!(!quiet.contains("\nStatistics\n"), "{quiet}");

        // Every number in a plan is read to choose between plans that produce the same rows, so
        // every line says decide. A line that said enable over a guess would be a bug, and the
        // point of printing the use is that it would be a visible one.
        let out = asked(plan, &[("t", 1000)]);
        for line in tree(&out) {
            assert!(line.contains(", read to decide]"), "{line}");
        }
    }

    #[test]
    fn the_statistics_section_counts_the_classes_and_says_which_uses_never_happened() {
        let out = asked(
            concat!(
                "Filter (#0.0::INTEGER > 1::INTEGER)::BOOLEAN\n",
                "  Get memory.main.t AS t #0 [a::INTEGER]\n",
            ),
            &[("t", 1000)],
        );
        assert!(out.contains("\nStatistics\n"), "{out}");
        // Two operators, so two reads: the scan off a counted row count and the filter off a
        // constant. Both had a number behind them, which is what the share is counting.
        assert!(
            out.contains(
                "  2 read to decide: exact 1, certified 0, estimated 1, unknown 0, \
                 100% of them with a number behind them\n"
            ),
            "{out}"
        );
        // The uses that did not happen are said out loud. An absence would read as an oversight,
        // and the whole value of this line is that nobody licensed a rewrite off a guess.
        assert!(out.contains("  nothing was read to answer or to enable\n"), "{out}");
    }

    #[test]
    fn a_plan_nobody_measured_says_so_in_the_section_as_well_as_on_the_lines() {
        let out = asked("Get memory.main.t AS t #0 [a::INTEGER]\n", &[]);
        assert!(out.contains("[rows unknown, read to decide]"), "{out}");
        assert!(
            out.contains(
                "  1 read to decide: exact 0, certified 0, estimated 0, unknown 1, \
                 0% of them with a number behind them\n"
            ),
            "{out}"
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
    fn the_document_gets_one_class_per_operator_and_the_number_that_goes_with_it() {
        let plan = parsed(concat!(
            "Filter (#0.0::INTEGER > 1::INTEGER)::BOOLEAN\n",
            "  Get memory.main.t AS t #0 [a::INTEGER]\n",
        ));
        let mut facts = Facts::new();
        facts.record("memory", "main", "t", 1000);
        let mut document = Document::new("select");
        let shape = Shape::of(&plan);
        for id in 0..shape.operators() {
            document.operators.push(Operator::new(id, 0, "operator"));
        }
        record_estimates(&plan, &facts, &mut document);

        // Two operators, so two decisions, and the histogram is a count of decisions rather than
        // of nodes or of rows.
        assert_eq!(document.estimates.total(), 2);
        assert_eq!(document.estimates.exact(), 1);
        assert_eq!(document.estimates.estimated(), 1);
        assert_eq!(document.estimates.unknown(), 0);
        // And the class on a row always agrees with the number on the same row, so a reader never
        // sees a class over a missing estimate or an estimate with no class on it.
        for operator in &document.operators {
            assert_eq!(operator.estimated_rows.is_some(), operator.estimate_class.is_some());
        }
    }

    #[test]
    fn an_operator_nobody_estimated_is_counted_as_nobody_knowing_rather_than_left_out() {
        // The share the series moves is a share of every decision, so a decision made with nothing
        // has to be in the denominator. Dropping it would make a planner that knows less look
        // better than one that knows more.
        let plan = parsed("TableFunction range args=[] #0 [a::BIGINT]\n");
        let mut document = Document::new("select");
        let shape = Shape::of(&plan);
        for id in 0..shape.operators() {
            document.operators.push(Operator::new(id, 0, "operator"));
        }
        record_estimates(&plan, &Facts::new(), &mut document);

        assert_eq!(document.estimates.unknown(), document.estimates.total());
        assert!(document.estimates.total() > 0);
        assert_eq!(document.operators[0].estimated_rows, None);
        assert_eq!(document.operators[0].estimate_class, None);
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
        assert!(lines[0].contains("[~5000 rows estimated from default]"), "{out}");
        assert!(
            lines[1].contains("small") && lines[1].contains("[10 rows exact from row count]"),
            "{out}"
        );
        assert!(
            lines[2].contains("big") && lines[2].contains("[5000 rows exact from row count]"),
            "{out}"
        );
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
        let out = explain_with(
            &plan,
            &Facts::new(),
            Seams::new(&settings, &registries),
            Statistics::NotAsked,
        );
        assert!(out.contains("[reference]"), "{out}");
        assert!(!out.contains("sort = "), "{out}");
    }
}
