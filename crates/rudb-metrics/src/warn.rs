//! The warnings, worked out from the document rather than written into it.
//!
//! A warning that something has to remember to raise is a warning that is missing on the day it
//! matters. Everything here is a rule over numbers that are in the document anyway, so a run that
//! spilled says so whether or not the operator that spilled thought to mention it, and a rule that
//! is wrong is wrong in one place.
//!
//! The order is deliberate. How the query ended comes first, because a partial run makes every
//! number below it partial. Then what it did that costs time, then what it got wrong, then what it
//! cannot account for.

use rudb_common::human;

use crate::document::{Document, Outcome};
use crate::{commas, duration};

/// How far an estimate can be out before it is worth saying so.
///
/// An order of magnitude. Anything less is the ordinary error of an estimate made without running
/// the query, and a warning that fires on an ordinary case is a warning people learn to skip.
const Q_ERROR: u128 = 10;

/// How much of a pipeline's wall time can be spent waiting before it is worth saying so.
///
/// Half. A pipeline that spends most of its time blocked is not a pipeline whose operators are
/// slow, and those are two different problems with two different fixes.
const BLOCKED_SHARE: u64 = 2;

/// How much of the CPU in the pipelines can go on driving them before it is worth saying so, in
/// percent.
///
/// A quarter. The loop that runs a pipeline takes a morsel, allocates the chunk its source fills,
/// hands it up the tree and drops it, and that is real work rather than the cost of measuring. A
/// query that spends more than a quarter of itself there is one whose operators are not what makes
/// it slow, and it is the first thing to look at before anybody optimizes one of them.
const DRIVING_SHARE: u64 = 25;

/// How close to the memory limit counts as against it, in percent.
const NEAR_LIMIT: u64 = 95;

/// How far the pipelines may be from the CPU time the execution took, in percent.
///
/// Five, which is the same tolerance `rudb-bench` applies to the same two numbers out of the
/// document this crate writes. Missing it means either that time is going somewhere no pipeline
/// covers or that the instrumentation counts something twice, and both are worth knowing.
const CPU_TOLERANCE: u64 = 5;

impl Document {
    /// Everything the engine knows it did badly, in the order worth reading it.
    #[must_use]
    pub fn warnings(&self) -> Vec<String> {
        let mut warnings = Vec::new();
        self.ending(&mut warnings);
        self.spills(&mut warnings);
        self.references(&mut warnings);
        self.estimates(&mut warnings);
        self.waiting(&mut warnings);
        self.driving(&mut warnings);
        self.limit(&mut warnings);
        self.accounting(&mut warnings);
        warnings
    }

    /// A run that stopped early is a run whose numbers cannot be compared with a whole one.
    fn ending(&self, warnings: &mut Vec<String>) {
        match &self.outcome {
            Outcome::Succeeded => {}
            Outcome::Cancelled => {
                warnings
                    .push("the query was cancelled, so every number here is partial".to_string());
            }
            Outcome::Failed(message) => warnings
                .push(format!("the query failed, so every number here is partial: {message}")),
        }
    }

    /// Spilling is the engine writing out what it could not hold, and it is the first thing to look
    /// at when a query is slower than the same query on a bigger machine.
    fn spills(&self, warnings: &mut Vec<String>) {
        for operator in self.operators.iter().filter(|operator| operator.bytes_spilled > 0) {
            warnings.push(format!(
                "{} spilled {}",
                operator.named(),
                human(operator.bytes_spilled)
            ));
        }
    }

    /// A reference implementation is the simplest correct version of something, kept so that the
    /// fast one has something to be tested against. One line naming how many rather than one line
    /// each, because for most of the first year the answer is all of them.
    fn references(&self, warnings: &mut Vec<String>) {
        let reference = self.operators.iter().filter(|operator| operator.reference_impl).count();
        if reference > 0 {
            warnings.push(format!(
                "{reference} of the {} operators ran a reference implementation, which is the slow path kept for differential testing",
                self.operators.len()
            ));
        }
    }

    /// An estimate an order of magnitude out is how a bad plan explains itself.
    fn estimates(&self, warnings: &mut Vec<String>) {
        for operator in &self.operators {
            let Some(estimated) = operator.estimated_rows else { continue };
            let (high, low) = if estimated > operator.rows_out {
                (u128::from(estimated), u128::from(operator.rows_out))
            } else {
                (u128::from(operator.rows_out), u128::from(estimated))
            };
            // A zero on either side is an estimate of nothing or a result of nothing, and dividing
            // by it says infinity when what it means is that one of the two is a special case. One
            // row is the floor, which makes the ratio the size of the other side.
            let low = low.max(1);
            if high / low < Q_ERROR {
                continue;
            }
            warnings.push(format!(
                "{} was estimated at {} rows and produced {} (q-error {})",
                operator.named(),
                commas(estimated),
                commas(operator.rows_out),
                ratio(high, low)
            ));
        }
    }

    /// A pipeline that spends its time waiting is not a pipeline whose operators are slow.
    fn waiting(&self, warnings: &mut Vec<String>) {
        for pipeline in &self.pipelines {
            let blocked = pipeline.blocked.total();
            if pipeline.wall_ns == 0 || blocked < pipeline.wall_ns / BLOCKED_SHARE {
                continue;
            }
            let (reason, _) = pipeline.blocked.largest();
            warnings.push(format!(
                "pipeline {} spent {} of its {} blocked, most of it on {reason}",
                pipeline.id,
                duration(blocked),
                duration(pipeline.wall_ns)
            ));
        }
    }

    /// A pipeline's time is its driver's time, and the difference between that and the operators in
    /// it is what the driving cost. Saying so once for the whole query rather than once per
    /// pipeline, because the answer is about how chunks move through a tree and that is the same
    /// answer for every pipeline in it.
    fn driving(&self, warnings: &mut Vec<String>) {
        let pipelines = self.pipelines.iter().fold(0u64, |sum, at| sum.saturating_add(at.cpu_ns));
        let operators = self.operators.iter().fold(0u64, |sum, at| sum.saturating_add(at.cpu_ns));
        let driving = pipelines.saturating_sub(operators);
        if pipelines == 0 || driving <= pipelines / 100 * DRIVING_SHARE {
            return;
        }
        warnings.push(format!(
            "{} of the {} of CPU in the pipelines went on driving them rather than on the operators in them",
            duration(driving),
            duration(pipelines)
        ));
    }

    /// A query that came within a few percent of its limit did not fail, and the next one on a
    /// slightly larger input will.
    fn limit(&self, warnings: &mut Vec<String>) {
        let Some(limit) = self.settings.memory_limit else { return };
        if limit == 0 || self.resource.peak_bytes < limit / 100 * NEAR_LIMIT {
            return;
        }
        warnings.push(format!(
            "the query peaked at {} against a limit of {}",
            human(self.resource.peak_bytes),
            human(limit)
        ));
    }

    /// The same cross check `rudb-bench` makes on this document, made here as well, because it
    /// needs nothing external and the engine should be the first to know.
    ///
    /// The pipelines against the execution, not the operators against the whole run. A pipeline is
    /// the unit that is driven, and its driver is charged for the loop as well as for the operator
    /// calls the loop makes, so the pipelines are what can add up to the execution and the
    /// operators are always short of it by whatever the driving cost. The build is off the right
    /// hand side for the same reason from the other end: opening a file and allocating a tree
    /// happen before there is a pipeline to charge.
    ///
    /// A document with no pipelines falls back to the operators, which is what a hand written one
    /// in a test has and what a caller that built a tree without a report gets.
    fn accounting(&self, warnings: &mut Vec<String>) {
        let total = self.resource.cpu_ns.saturating_sub(self.resource.build_cpu_ns);
        if self.operators.is_empty() || total == 0 {
            return;
        }
        let (what, counted) = if self.pipelines.is_empty() {
            ("operators", self.operators.iter().fold(0u64, |sum, at| sum.saturating_add(at.cpu_ns)))
        } else {
            ("pipelines", self.pipelines.iter().fold(0u64, |sum, at| sum.saturating_add(at.cpu_ns)))
        };
        let gap = counted.abs_diff(total);
        if gap <= total / 100 * CPU_TOLERANCE {
            return;
        }
        warnings.push(format!(
            "the {what} account for {} of the {} of CPU this query spent executing",
            duration(counted),
            duration(total)
        ));
    }
}

/// One number over another, to one decimal place.
///
/// Integer arithmetic because a q-error of 17.5 is a label rather than a measurement, and because
/// a rounded ratio printed from a float is one more place for a number to change between two
/// machines that should agree.
fn ratio(high: u128, low: u128) -> String {
    let tenths = (high * 10 + low / 2) / low;
    format!("{}.{}", tenths / 10, tenths % 10)
}

#[cfg(test)]
mod tests {
    use crate::document::{Document, Operator, Outcome, Pipeline};

    fn document() -> Document {
        Document::new("select 1")
    }

    #[test]
    fn a_clean_run_has_nothing_to_say() {
        let mut metrics = document();
        metrics.operators.push(Operator::new(0, 0, "Scan"));
        assert!(metrics.warnings().is_empty());
    }

    #[test]
    fn a_cancelled_run_says_so_first() {
        let mut metrics = document();
        metrics.outcome = Outcome::Cancelled;
        let mut scan = Operator::new(0, 0, "Scan");
        scan.bytes_spilled = 2048;
        metrics.operators.push(scan);
        let warnings = metrics.warnings();
        assert_eq!(warnings[0], "the query was cancelled, so every number here is partial");
        assert_eq!(warnings[1], "operator 0 (Scan) spilled 2.0 KiB");
    }

    #[test]
    fn a_failed_run_carries_the_message() {
        let mut metrics = document();
        metrics.outcome = Outcome::Failed("out of memory".to_string());
        assert_eq!(
            metrics.warnings(),
            vec!["the query failed, so every number here is partial: out of memory"]
        );
    }

    #[test]
    fn reference_implementations_are_counted_rather_than_listed() {
        let mut metrics = document();
        for id in 0..3 {
            let mut operator = Operator::new(id, 0, "Sort");
            operator.reference_impl = id < 2;
            metrics.operators.push(operator);
        }
        assert_eq!(
            metrics.warnings(),
            vec![
                "2 of the 3 operators ran a reference implementation, which is the slow path kept for differential testing"
            ]
        );
    }

    #[test]
    fn an_estimate_an_order_of_magnitude_out_is_a_warning() {
        let mut metrics = document();
        let mut close = Operator::new(0, 0, "Filter");
        close.estimated_rows = Some(1000);
        close.rows_out = 4000;
        let mut wrong = Operator::new(1, 0, "HashAggregate");
        wrong.estimated_rows = Some(2_400_000);
        wrong.rows_out = 41_983_110;
        metrics.operators.extend([close, wrong]);
        assert_eq!(
            metrics.warnings(),
            vec![
                "operator 1 (HashAggregate) was estimated at 2,400,000 rows and produced 41,983,110 (q-error 17.5)"
            ]
        );
    }

    #[test]
    fn an_estimate_against_no_rows_does_not_divide_by_zero() {
        let mut metrics = document();
        let mut empty = Operator::new(0, 0, "Filter");
        empty.estimated_rows = Some(50);
        empty.rows_out = 0;
        metrics.operators.push(empty);
        assert_eq!(
            metrics.warnings(),
            vec!["operator 0 (Filter) was estimated at 50 rows and produced 0 (q-error 50.0)"]
        );
    }

    #[test]
    fn a_pipeline_that_mostly_waited_says_what_it_waited_on() {
        let mut metrics = document();
        let mut pipeline = Pipeline::new(0);
        pipeline.wall_ns = 1_000_000_000;
        pipeline.blocked.io_ns = 600_000_000;
        pipeline.blocked.memory_ns = 1_000_000;
        metrics.pipelines.push(pipeline);
        assert_eq!(
            metrics.warnings(),
            vec!["pipeline 0 spent 601ms of its 1.000s blocked, most of it on io"]
        );
    }

    #[test]
    fn coming_within_a_few_percent_of_the_limit_is_worth_knowing() {
        let mut metrics = document();
        metrics.settings.memory_limit = Some(1 << 30);
        metrics.resource.peak_bytes = 1 << 30;
        assert_eq!(
            metrics.warnings(),
            vec!["the query peaked at 1.0 GiB against a limit of 1.0 GiB"]
        );
    }

    #[test]
    fn cpu_the_operators_do_not_account_for_is_reported() {
        let mut metrics = document();
        metrics.resource.cpu_ns = 10_000_000_000;
        let mut scan = Operator::new(0, 0, "Scan");
        scan.cpu_ns = 4_000_000_000;
        metrics.operators.push(scan);
        assert_eq!(
            metrics.warnings(),
            vec![
                "the operators account for 4.000s of the 10.000s of CPU this query spent executing"
            ]
        );
    }

    #[test]
    fn a_document_with_pipelines_is_checked_against_the_pipelines() {
        let mut metrics = document();
        metrics.resource.cpu_ns = 11_000_000_000;
        metrics.resource.build_cpu_ns = 1_000_000_000;
        let mut scan = Operator::new(0, 0, "Scan");
        scan.cpu_ns = 9_000_000_000;
        metrics.operators.push(scan);
        let mut pipeline = Pipeline::new(0);
        pipeline.cpu_ns = 9_800_000_000;
        metrics.pipelines.push(pipeline);
        assert!(
            metrics.warnings().is_empty(),
            "the operator is short of the execution and the pipeline that drove it is not"
        );
    }

    #[test]
    fn the_time_spent_building_is_not_time_the_pipelines_have_to_account_for() {
        let mut metrics = document();
        metrics.resource.cpu_ns = 10_000_000_000;
        metrics.resource.build_cpu_ns = 4_000_000_000;
        let mut scan = Operator::new(0, 0, "Scan");
        scan.cpu_ns = 5_600_000_000;
        metrics.operators.push(scan);
        let mut pipeline = Pipeline::new(0);
        pipeline.cpu_ns = 6_000_000_000;
        metrics.pipelines.push(pipeline);
        assert!(metrics.warnings().is_empty());
    }

    #[test]
    fn a_query_that_spent_its_time_driving_rather_than_in_its_operators_says_so() {
        let mut metrics = document();
        metrics.resource.cpu_ns = 10_000_000_000;
        let mut scan = Operator::new(0, 0, "Scan");
        scan.cpu_ns = 6_000_000_000;
        metrics.operators.push(scan);
        let mut pipeline = Pipeline::new(0);
        pipeline.cpu_ns = 10_000_000_000;
        metrics.pipelines.push(pipeline);
        assert_eq!(
            metrics.warnings(),
            vec![
                "4.000s of the 10.000s of CPU in the pipelines went on driving them rather than on the operators in them"
            ]
        );
    }

    #[test]
    fn cpu_within_the_tolerance_is_not_reported() {
        let mut metrics = document();
        metrics.resource.cpu_ns = 10_000_000_000;
        let mut scan = Operator::new(0, 0, "Scan");
        scan.cpu_ns = 9_600_000_000;
        metrics.operators.push(scan);
        assert!(metrics.warnings().is_empty());
    }
}
