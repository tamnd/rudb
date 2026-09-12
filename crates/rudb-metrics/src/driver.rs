//! What the loop that runs a pipeline counts, which is not what any of its operators count.
//!
//! An operator is measured around its own call, so a chunk that goes through four operators is
//! charged to four operators and nothing charges the loop that made the four calls. That loop is
//! real work. It takes a morsel, allocates the chunk the source fills, hands the chunk up the tree,
//! drops it and goes round again, and on a query that moves ten thousand chunks it is a third of
//! the execution. Left uncounted it shows up as CPU time that went missing, which is the one thing
//! the cross check in `rudb-bench` exists to catch, and it would be caught as a bug in the
//! instrumentation every time rather than as the ordinary cost of driving a tree.
//!
//! So the driver is measured too, and a pipeline's time is its driver's time rather than the sum of
//! its operators'. The difference between the two is what the driving cost, and the run report
//! prints it in its own column.
//!
//! # Time inside time
//!
//! Pipelines run inside each other here. A sort is drained by the loop above it, so the loop above
//! it is still running while the loop below it is, and a span around each of them would count the
//! same nanosecond twice. What a driver charges itself is therefore its span minus whatever was
//! charged while its span was open, which is exactly what the pipelines underneath it charged. Every
//! driver of one report shares the running total that makes that subtraction possible, which is why
//! a [`Driver`] comes from a [`Report`](crate::Report) rather than being made on its own.
//!
//! Summing exclusive times adds up to the outermost span, and at F0 the outermost span is the
//! execution, so the cross check `rudb-bench` makes on the document is close to an identity here.
//! That is worth saying plainly. It is a fact about a tree that runs on one thread inside one loop,
//! not a fact about the arithmetic, and the arithmetic is the reason it is worth having anyway:
//! what every driver reports is measured, rather than worked out by taking everything else off the
//! number it is about to be compared against. A pipeline nobody drove falls back to the sum of its
//! operators and comes out short. Time spent inside the execution and outside every driver shows up
//! as a gap. And on the day pipelines run on threads of their own, the execution span stops
//! containing them and the check has something to say again.

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use crate::clock::Span;

/// The running total every driver of one report adds its own time to.
///
/// Relaxed like every other counter here. Nothing reads it to decide anything: it is read at the
/// start and the end of a span on the same thread that writes it, and the day pipelines run on
/// several threads at once a driver's span and the spans nested in it are still one thread's.
#[derive(Debug, Default)]
pub(crate) struct Charged {
    wall_ns: AtomicU64,
    cpu_ns: AtomicU64,
}

impl Charged {
    /// What every driver has charged itself so far.
    fn now(&self) -> (u64, u64) {
        (self.wall_ns.load(Ordering::Relaxed), self.cpu_ns.load(Ordering::Relaxed))
    }

    /// Adds one driver's own time.
    fn add(&self, wall_ns: u64, cpu_ns: u64) {
        self.wall_ns.fetch_add(wall_ns, Ordering::Relaxed);
        self.cpu_ns.fetch_add(cpu_ns, Ordering::Relaxed);
    }
}

/// The counters for the loop that runs one pipeline.
#[derive(Debug)]
pub struct Driver {
    pipeline: u32,
    wall_ns: AtomicU64,
    cpu_ns: AtomicU64,
    charged: Arc<Charged>,
}

impl Driver {
    /// The driver of this pipeline, sharing the running total of `charged`.
    pub(crate) fn new(pipeline: u32, charged: Arc<Charged>) -> Self {
        Self { pipeline, wall_ns: AtomicU64::new(0), cpu_ns: AtomicU64::new(0), charged }
    }

    /// The pipeline this drives.
    #[must_use]
    pub fn pipeline(&self) -> u32 {
        self.pipeline
    }

    /// Starts timing a run of this pipeline.
    ///
    /// The charge happens when the returned handle goes away, whether that is at the end of the
    /// loop or on the way out of an error, because a query that failed halfway through is the query
    /// somebody most wants the numbers of.
    #[must_use]
    pub fn running(&self) -> Running<'_> {
        Running { driver: self, before: self.charged.now(), span: Some(Span::start()) }
    }

    /// The wall and CPU nanoseconds this driver has charged itself.
    #[must_use]
    pub fn spent(&self) -> (u64, u64) {
        (self.wall_ns.load(Ordering::Relaxed), self.cpu_ns.load(Ordering::Relaxed))
    }

    /// Charges a span, taking off what the pipelines inside it charged while it was open.
    fn charge(&self, before: (u64, u64), span: (u64, u64)) {
        let now = self.charged.now();
        let wall = span.0.saturating_sub(now.0.saturating_sub(before.0));
        let cpu = span.1.saturating_sub(now.1.saturating_sub(before.1));
        self.wall_ns.fetch_add(wall, Ordering::Relaxed);
        self.cpu_ns.fetch_add(cpu, Ordering::Relaxed);
        self.charged.add(wall, cpu);
    }
}

/// One run of one pipeline, being timed.
#[derive(Debug)]
pub struct Running<'a> {
    driver: &'a Driver,
    /// What every driver had charged when this started, which is what the subtraction is against.
    before: (u64, u64),
    /// None once the charge has happened, so that stopping and then dropping charges once.
    span: Option<Span>,
}

impl Running<'_> {
    /// Stops timing and reports the whole span, including the pipelines that ran inside it.
    ///
    /// What the driver keeps is less than this. The caller that wants the whole span is the one
    /// timing the execution rather than the pipeline, and it is the same pair of clock readings, so
    /// handing it back here is cheaper and more honest than reading the clock twice around the same
    /// loop and reporting two numbers for it.
    pub fn stop(mut self) -> (u64, u64) {
        self.done().unwrap_or((0, 0))
    }

    /// Charges the driver, once.
    fn done(&mut self) -> Option<(u64, u64)> {
        let span = self.span.take()?.stop();
        self.driver.charge(self.before, span);
        Some(span)
    }
}

impl Drop for Running<'_> {
    fn drop(&mut self) {
        self.done();
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use super::{Charged, Driver};

    /// Work worth a measurable number of nanoseconds.
    fn spin() {
        let mut counted: u64 = 0;
        for at in 0..200_000u64 {
            counted = counted.wrapping_add(at * at);
        }
        assert!(counted > 0, "the loop has to be kept");
    }

    #[test]
    fn a_driver_charges_itself_what_it_did() {
        let charged = Arc::new(Charged::default());
        let driver = Driver::new(0, charged);
        assert_eq!(driver.spent(), (0, 0));
        let whole = driver.running().stop();
        assert_eq!(driver.spent().0, whole.0, "nothing ran inside it, so it kept all of it");
    }

    #[test]
    fn a_driver_does_not_charge_itself_what_ran_inside_it() {
        let charged = Arc::new(Charged::default());
        let outer = Driver::new(0, Arc::clone(&charged));
        let inner = Driver::new(1, Arc::clone(&charged));
        let whole = {
            let running = outer.running();
            spin();
            {
                let nested = inner.running();
                spin();
                nested.stop();
            }
            running.stop()
        };
        let (outer_wall, _) = outer.spent();
        let (inner_wall, _) = inner.spent();
        assert!(inner_wall > 0, "the nested driver did something");
        assert!(outer_wall > 0, "and so did the one around it");
        assert_eq!(
            outer_wall + inner_wall,
            whole.0,
            "the two exclusive times add up to the one span around both"
        );
    }

    #[test]
    fn a_driver_run_twice_adds_up() {
        let charged = Arc::new(Charged::default());
        let driver = Driver::new(0, charged);
        let first = driver.running().stop();
        let second = driver.running().stop();
        assert_eq!(driver.spent().0, first.0 + second.0);
    }

    /// A function that gives up while it is being timed, which is what an operator returning an
    /// error looks like from the driver's side.
    fn gives_up(driver: &Driver) -> Result<(), ()> {
        let _running = driver.running();
        spin();
        Err(())
    }

    #[test]
    fn a_span_dropped_on_the_way_out_of_an_error_is_still_charged() {
        let charged = Arc::new(Charged::default());
        let driver = Driver::new(0, charged);
        assert!(gives_up(&driver).is_err());
        assert!(driver.spent().0 > 0, "the query stopped and the time it took did not go away");
    }
}
