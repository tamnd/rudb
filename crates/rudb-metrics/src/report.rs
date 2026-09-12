//! What the code that builds an execution keeps, so that the numbers can be collected at the end.
//!
//! The shim counts into an [`Counters`](crate::Counters) that lives as long as the operator it
//! wraps. Something has to hold the other end of every one of those, or the counting happens and
//! nobody reads it, and that something is this. The builder registers each operator as it makes it
//! and declares each pipeline as it breaks one, and when the query is over [`Report::fill`] turns
//! the lot into the rows of a document.
//!
//! It is behind a lock because the builder is single threaded today and will not be forever, and
//! because the alternative is to hand the builder a mutable borrow it would have to thread through
//! every arm of a recursive match. The lock is taken once per operator at build time and once at
//! the end, never on the path a chunk takes, so what it costs is nothing worth measuring.

use std::sync::{Arc, Mutex, PoisonError};

use crate::counters::Counters;
use crate::document::{Document, Pipeline};

/// The counters and the pipeline rows of one execution.
#[derive(Debug, Default)]
pub struct Report {
    kept: Mutex<Kept>,
}

/// The two lists, which are only ever locked together.
#[derive(Debug, Default)]
struct Kept {
    operators: Vec<Arc<Counters>>,
    pipelines: Vec<Pipeline>,
}

impl Report {
    /// An empty report.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Registers an operator's counters and hands back the handle to wrap it with.
    ///
    /// The report keeps one reference and the operator keeps the other, which is why this gives
    /// back what it was given rather than taking it away.
    #[must_use]
    pub fn watch(&self, counters: Counters) -> Arc<Counters> {
        let counters = Arc::new(counters);
        self.locked().operators.push(Arc::clone(&counters));
        counters
    }

    /// Declares that a pipeline with this id exists.
    ///
    /// Called for the root pipeline, which nothing depends on and which therefore no edge would
    /// otherwise mention. Declaring one twice is not an error and does nothing the second time.
    pub fn pipeline(&self, id: u32) {
        let mut kept = self.locked();
        row(&mut kept.pipelines, id);
    }

    /// Declares that `pipeline` cannot start until `on` has finished.
    ///
    /// Both rows are made if they are not there yet, so a builder that walks down the tree only has
    /// to say what it just broke and never has to say it twice.
    pub fn depends(&self, pipeline: u32, on: u32) {
        let mut kept = self.locked();
        row(&mut kept.pipelines, on);
        let parent = row(&mut kept.pipelines, pipeline);
        if !parent.depends_on.contains(&on) {
            parent.depends_on.push(on);
        }
    }

    /// Writes the operator rows and the pipeline rows into a document.
    ///
    /// Both lists come out sorted by id, because a document is read by a person as often as by a
    /// parser and build order is not an order anybody reading it expects.
    ///
    /// A pipeline's time is the sum of the time of its operators. That is right while a pipeline is
    /// one instance on one thread, and it stops being right the day there are several, which is
    /// when the driver starts reporting its own wall time and this stops deriving it. The waiting
    /// stays at zero here for the same reason: nothing waits yet, and a number made up to fill a
    /// field is worse than a zero that means nothing happened.
    pub fn fill(&self, document: &mut Document) {
        let kept = self.locked();
        let mut operators: Vec<_> =
            kept.operators.iter().map(|counters| counters.snapshot()).collect();
        operators.sort_by_key(|operator| operator.id);
        let mut pipelines = kept.pipelines.clone();
        for pipeline in &mut pipelines {
            pipeline.depends_on.sort_unstable();
            for operator in operators.iter().filter(|operator| operator.pipeline == pipeline.id) {
                pipeline.wall_ns = pipeline.wall_ns.saturating_add(operator.wall_ns);
                pipeline.cpu_ns = pipeline.cpu_ns.saturating_add(operator.cpu_ns);
            }
        }
        pipelines.sort_by_key(|pipeline| pipeline.id);
        document.pipelines = pipelines;
        document.operators = operators;
    }

    /// The lists, through a lock that is taken rather than reported on.
    ///
    /// A poisoned lock means a thread panicked while it held this, and what it held is two vectors
    /// of plain data rather than an invariant somebody was halfway through breaking. Refusing to
    /// report the numbers of a query that panicked would lose exactly the numbers somebody wants.
    fn locked(&self) -> std::sync::MutexGuard<'_, Kept> {
        self.kept.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

/// The row for this pipeline, made if it is not there.
fn row(pipelines: &mut Vec<Pipeline>, id: u32) -> &mut Pipeline {
    if let Some(at) = pipelines.iter().position(|pipeline| pipeline.id == id) {
        return &mut pipelines[at];
    }
    pipelines.push(Pipeline::new(id));
    pipelines.last_mut().expect("a row that was just pushed is there")
}

#[cfg(test)]
mod tests {
    use super::Report;
    use crate::counters::Counters;
    use crate::document::Document;

    #[test]
    fn a_report_becomes_the_rows_of_a_document() {
        let report = Report::new();
        report.pipeline(0);
        report.depends(0, 1);
        let scan = report.watch(Counters::new(2, 1, "Scan"));
        let sort = report.watch(Counters::new(1, 1, "Sort"));
        let read = report.watch(Counters::new(0, 0, "Buffered"));
        scan.made(1000);
        scan.spent(400, 380);
        sort.took(1000);
        sort.spent(600, 590);
        read.made(1000);
        read.spent(100, 90);
        let mut document = Document::new("select * from t order by a");
        report.fill(&mut document);
        assert_eq!(document.operators.len(), 3);
        assert_eq!(document.operators[0].kind, "Buffered");
        assert_eq!(document.operators[1].kind, "Sort");
        assert_eq!(document.operators[2].kind, "Scan");
        assert_eq!(document.pipelines.len(), 2);
        assert_eq!(document.pipelines[0].depends_on, vec![1]);
        assert_eq!(document.pipelines[0].wall_ns, 100);
        assert!(document.pipelines[1].depends_on.is_empty());
        assert_eq!(document.pipelines[1].wall_ns, 1000);
        assert_eq!(document.pipelines[1].cpu_ns, 970);
    }

    #[test]
    fn an_edge_declared_twice_is_one_edge() {
        let report = Report::new();
        report.depends(0, 1);
        report.depends(0, 1);
        report.pipeline(0);
        let mut document = Document::new("select 1");
        report.fill(&mut document);
        assert_eq!(document.pipelines.len(), 2);
        assert_eq!(document.pipelines[0].depends_on, vec![1]);
    }

    #[test]
    fn an_execution_that_measured_nothing_has_no_rows() {
        let mut document = Document::new("select 1");
        Report::new().fill(&mut document);
        assert!(document.operators.is_empty());
        assert!(document.pipelines.is_empty());
    }
}
