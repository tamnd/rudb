//! What one operator counts while it runs.
//!
//! An operator is shared across every thread running its pipeline, so its counters are shared too
//! and every one of them is an atomic. The ordering is relaxed everywhere, which is the right
//! answer rather than a shortcut: nothing reads a counter to decide anything, they are read once at
//! the end after every thread has finished, and an ordering strong enough to make a partial read
//! meaningful would cost something on every chunk to make a number nobody looks at correct.
//!
//! What is not atomic is the part that does not change: the id, the pipeline, the kind, the
//! estimate the optimizer made and whether this is a reference implementation are all known when
//! the operator is built and none of them move afterwards.

use std::sync::atomic::{AtomicU64, Ordering};

use crate::document::{Memory, Operator};

/// The counters for one operator.
#[derive(Debug)]
pub struct Counters {
    id: u32,
    pipeline: u32,
    kind: String,
    detail: Option<String>,
    estimated_rows: Option<u64>,
    reference_impl: bool,
    rows_in: AtomicU64,
    rows_out: AtomicU64,
    wall_ns: AtomicU64,
    cpu_ns: AtomicU64,
    bytes_read: AtomicU64,
    bytes_decoded: AtomicU64,
    bytes_spilled: AtomicU64,
    reserved: AtomicU64,
    high_water: AtomicU64,
}

impl Counters {
    /// The counters for an operator of this kind, in this pipeline, with everything at zero.
    #[must_use]
    pub fn new(id: u32, pipeline: u32, kind: &str) -> Self {
        Self {
            id,
            pipeline,
            kind: kind.to_string(),
            detail: None,
            estimated_rows: None,
            reference_impl: false,
            rows_in: AtomicU64::new(0),
            rows_out: AtomicU64::new(0),
            wall_ns: AtomicU64::new(0),
            cpu_ns: AtomicU64::new(0),
            bytes_read: AtomicU64::new(0),
            bytes_decoded: AtomicU64::new(0),
            bytes_spilled: AtomicU64::new(0),
            reserved: AtomicU64::new(0),
            high_water: AtomicU64::new(0),
        }
    }

    /// The part of the operator worth printing beside its kind, such as the table or the keys.
    #[must_use]
    pub fn detailed(mut self, detail: &str) -> Self {
        self.detail = Some(detail.to_string());
        self
    }

    /// What the optimizer thought this operator would produce.
    #[must_use]
    pub fn estimated(mut self, rows: u64) -> Self {
        self.estimated_rows = Some(rows);
        self
    }

    /// That what runs here is the reference implementation rather than a fast one.
    #[must_use]
    pub fn reference(mut self) -> Self {
        self.reference_impl = true;
        self
    }

    /// Rows handed to this operator.
    pub fn took(&self, rows: u64) {
        self.rows_in.fetch_add(rows, Ordering::Relaxed);
    }

    /// Rows this operator produced.
    pub fn made(&self, rows: u64) {
        self.rows_out.fetch_add(rows, Ordering::Relaxed);
    }

    /// What one call cost, which is what [`Span::stop`](crate::Span::stop) reports.
    pub fn spent(&self, wall_ns: u64, cpu_ns: u64) {
        self.wall_ns.fetch_add(wall_ns, Ordering::Relaxed);
        self.cpu_ns.fetch_add(cpu_ns, Ordering::Relaxed);
    }

    /// Bytes read from a file or a block device.
    pub fn read(&self, bytes: u64) {
        self.bytes_read.fetch_add(bytes, Ordering::Relaxed);
    }

    /// Bytes turned from a stored form into vectors.
    pub fn decoded(&self, bytes: u64) {
        self.bytes_decoded.fetch_add(bytes, Ordering::Relaxed);
    }

    /// Bytes written out to make room.
    pub fn spilled(&self, bytes: u64) {
        self.bytes_spilled.fetch_add(bytes, Ordering::Relaxed);
    }

    /// What this operator holds now, which also moves the high water mark when it is a new most.
    ///
    /// Reported rather than added, because memory is a level and not a total. An operator that
    /// reserves a megabyte and gives it back twice has held one megabyte, and a counter that added
    /// would say two.
    pub fn holding(&self, bytes: u64) {
        self.reserved.store(bytes, Ordering::Relaxed);
        self.high_water.fetch_max(bytes, Ordering::Relaxed);
    }

    /// The row this operator contributes to the document.
    #[must_use]
    pub fn snapshot(&self) -> Operator {
        let mut operator = Operator::new(self.id, self.pipeline, &self.kind);
        operator.detail.clone_from(&self.detail);
        operator.estimated_rows = self.estimated_rows;
        operator.reference_impl = self.reference_impl;
        operator.rows_in = self.rows_in.load(Ordering::Relaxed);
        operator.rows_out = self.rows_out.load(Ordering::Relaxed);
        operator.wall_ns = self.wall_ns.load(Ordering::Relaxed);
        operator.cpu_ns = self.cpu_ns.load(Ordering::Relaxed);
        operator.bytes_read = self.bytes_read.load(Ordering::Relaxed);
        operator.bytes_decoded = self.bytes_decoded.load(Ordering::Relaxed);
        operator.bytes_spilled = self.bytes_spilled.load(Ordering::Relaxed);
        operator.memory = Memory {
            reserved: self.reserved.load(Ordering::Relaxed),
            high_water: self.high_water.load(Ordering::Relaxed),
        };
        operator
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;
    use std::thread;

    use super::Counters;

    #[test]
    fn a_snapshot_carries_what_was_counted_and_what_was_known() {
        let counters = Counters::new(3, 0, "Scan").detailed("hits").estimated(1000).reference();
        counters.took(0);
        counters.made(2048);
        counters.spent(620_000, 600_000);
        counters.read(4096);
        counters.decoded(8192);
        counters.spilled(16);
        let operator = counters.snapshot();
        assert_eq!(operator.id, 3);
        assert_eq!(operator.kind, "Scan");
        assert_eq!(operator.detail.as_deref(), Some("hits"));
        assert_eq!(operator.estimated_rows, Some(1000));
        assert!(operator.reference_impl);
        assert_eq!(operator.rows_out, 2048);
        assert_eq!(operator.wall_ns, 620_000);
        assert_eq!(operator.cpu_ns, 600_000);
        assert_eq!(operator.bytes_read, 4096);
        assert_eq!(operator.bytes_decoded, 8192);
        assert_eq!(operator.bytes_spilled, 16);
    }

    #[test]
    fn memory_is_a_level_and_the_high_water_mark_remembers_the_most_of_it() {
        let counters = Counters::new(0, 0, "Sort");
        counters.holding(1000);
        counters.holding(4000);
        counters.holding(0);
        let operator = counters.snapshot();
        assert_eq!(operator.memory.reserved, 0);
        assert_eq!(operator.memory.high_water, 4000);
    }

    #[test]
    fn every_thread_counts_into_the_same_operator() {
        let counters = Arc::new(Counters::new(0, 0, "Filter"));
        let threads: Vec<_> = (0..8)
            .map(|_| {
                let counters = Arc::clone(&counters);
                thread::spawn(move || {
                    for _ in 0..1000 {
                        counters.took(2);
                        counters.made(1);
                        counters.spent(10, 8);
                    }
                })
            })
            .collect();
        for thread in threads {
            thread.join().expect("no counting thread panics");
        }
        let operator = counters.snapshot();
        assert_eq!(operator.rows_in, 16_000);
        assert_eq!(operator.rows_out, 8_000);
        assert_eq!(operator.wall_ns, 80_000);
        assert_eq!(operator.cpu_ns, 64_000);
    }
}
