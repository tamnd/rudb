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

use std::sync::OnceLock;
use std::sync::atomic::{AtomicU64, Ordering};

use rudb_common::{Cause, Spent, Stage, Tally};

use crate::document::{Implementation, Joined, Memory, Operator};

/// The counters for one operator.
#[derive(Debug)]
pub struct Counters {
    id: u32,
    pipeline: u32,
    kind: String,
    detail: Option<String>,
    estimated_rows: Option<u64>,
    implementations: Vec<Implementation>,
    /// Whether the shim around this operator reads the thread clock as well as the wall clock.
    ///
    /// Off unless somebody asked for the numbers, because the thread clock is a system call and the
    /// shim reads it twice per chunk. See [`Span`](crate::Span) for what that came to. Not an atomic
    /// because it is decided when the operator is built and read on every thread afterwards.
    charges_cpu: bool,
    rows_in: AtomicU64,
    rows_out: AtomicU64,
    wall_ns: AtomicU64,
    cpu_ns: AtomicU64,
    bytes_read: AtomicU64,
    bytes_decoded: AtomicU64,
    bytes_spilled: AtomicU64,
    /// Parts of the table read, and parts the statistics ruled out. See
    /// [`Operator::parts_pruned`](crate::Operator::parts_pruned) for why the second one is here.
    parts_read: AtomicU64,
    parts_pruned: AtomicU64,
    reserved: AtomicU64,
    high_water: AtomicU64,
    /// One counter per [`Cause`], in the order [`Cause::ALL`] lists them.
    ///
    /// A fixed array rather than a map, because the shim adds to this around every operator call
    /// and a map would be a hash per call to store a number that is almost always zero.
    fallbacks: [AtomicU64; Cause::ALL.len()],
    /// What a join did, for an operator that is one.
    ///
    /// Not an atomic and not a lock, because it is written once by whichever instance built the
    /// gathered side and read once at the end. Every other instance of that operator finds the
    /// table already built and has nothing to say that the first one did not.
    joined: OnceLock<Joined>,
    /// One clock and one byte count per [`Stage`], in the order [`Stage::ALL`] lists them.
    ///
    /// Only a scan fills these in. Everything else reports a row of zeroes, which costs nothing to
    /// carry and means the split is there the day another reader starts charging itself.
    stages: [AtomicU64; Stage::ALL.len()],
    stage_bytes: [AtomicU64; Stage::ALL.len()],
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
            implementations: Vec::new(),
            charges_cpu: false,
            rows_in: AtomicU64::new(0),
            rows_out: AtomicU64::new(0),
            wall_ns: AtomicU64::new(0),
            cpu_ns: AtomicU64::new(0),
            parts_read: AtomicU64::new(0),
            parts_pruned: AtomicU64::new(0),
            bytes_read: AtomicU64::new(0),
            bytes_decoded: AtomicU64::new(0),
            bytes_spilled: AtomicU64::new(0),
            reserved: AtomicU64::new(0),
            high_water: AtomicU64::new(0),
            joined: OnceLock::new(),
            fallbacks: [const { AtomicU64::new(0) }; Cause::ALL.len()],
            stages: [const { AtomicU64::new(0) }; Stage::ALL.len()],
            stage_bytes: [const { AtomicU64::new(0) }; Stage::ALL.len()],
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

    /// Says this operator's row wants a CPU column, so the shim around it reads the thread clock.
    ///
    /// Whoever builds the operator decides, because that is the one place that can see whether the
    /// statement is an `EXPLAIN ANALYZE` or ran under `enable_profiling`. Everything else gets the
    /// wall clock alone, and a wall clock per operator plus a thread clock per pipeline is enough to
    /// find a slow operator without paying a system call per chunk to do it.
    #[must_use]
    pub fn charging_cpu(mut self, charges: bool) -> Self {
        self.charges_cpu = charges;
        self
    }

    /// Whether the shim should read the thread clock around this operator.
    #[must_use]
    pub fn charges_cpu(&self) -> bool {
        self.charges_cpu
    }

    /// What the operator picked at one of the seams it sits on.
    ///
    /// Called once per seam with a registry, by whoever built the operator, because that is the
    /// only place that can see both the plan node and the settings the statement runs under. A
    /// seam with nothing registered is not reported, since there was nothing to choose between and
    /// a row saying so would be a row about the project rather than about the query.
    ///
    /// This replaced a flag that was set to true on every operator unconditionally, which meant
    /// every ClickBench run this project has published said 41 of 41 operators ran a reference
    /// implementation whatever the seams had actually done. A field that cannot disagree with
    /// itself is a field nobody should read, and it was read.
    #[must_use]
    pub fn chose(mut self, seam: &str, name: &str, is_reference: bool) -> Self {
        self.implementations.push(Implementation {
            seam: seam.to_string(),
            name: name.to_string(),
            is_reference,
        });
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

    /// One part of the table read rather than ruled out.
    pub fn part_read(&self) {
        self.parts_read.fetch_add(1, Ordering::Relaxed);
    }

    /// One part the statistics ruled out, so nothing in it was read.
    pub fn part_pruned(&self) {
        self.parts_pruned.fetch_add(1, Ordering::Relaxed);
    }

    /// Bytes turned from a stored form into vectors.
    pub fn decoded(&self, bytes: u64) {
        self.bytes_decoded.fetch_add(bytes, Ordering::Relaxed);
    }

    /// Bytes written out to make room.
    pub fn spilled(&self, bytes: u64) {
        self.bytes_spilled.fetch_add(bytes, Ordering::Relaxed);
    }

    /// Slow paths taken inside one call of this operator.
    ///
    /// The shim reads [`rudb_common::slow::here`] before the call and after it and hands over the
    /// difference, so what arrives here is what that operator did on that thread and nothing else.
    /// Adding rather than storing, because the operator is called once per chunk and the number
    /// worth having is the one for the whole query.
    pub fn fell_back(&self, tally: Tally) {
        if tally.is_empty() {
            return;
        }
        for (cause, times) in tally.taken() {
            self.fallbacks[cause.slot()].fetch_add(times, Ordering::Relaxed);
        }
    }

    /// Time spent in each stage of a read inside one call of this operator.
    ///
    /// The same shape as [`Self::fell_back`] and for the same reason: the shim takes a reading on
    /// either side of the call and hands over the difference, so what lands here is what this
    /// operator did on this thread. An operator that reads nothing hands over a row of zeroes and
    /// returns immediately.
    pub fn spent_reading(&self, spent: Spent) {
        if spent.is_empty() {
            return;
        }
        for (stage, nanos, bytes) in spent.taken() {
            self.stages[stage.slot()].fetch_add(nanos, Ordering::Relaxed);
            self.stage_bytes[stage.slot()].fetch_add(bytes, Ordering::Relaxed);
            if matches!(stage, Stage::Decode | Stage::Dictionary) {
                self.bytes_decoded.fetch_add(bytes, Ordering::Relaxed);
            }
        }
    }

    /// What a join chose, what it did not choose and what the gathered side came to.
    ///
    /// Called by the operator rather than by the shim around it, for the reason the byte counts
    /// are: a wrapper sees chunks going past and cannot see which of two inputs one came from,
    /// let alone which algorithm read it. Called where the table is built, because that is the one
    /// line in the engine where all four of these numbers are settled and in hand.
    ///
    /// The first call wins and the rest are dropped. Every instance of the operator runs this
    /// code and exactly one of them builds the table, so the ones that arrive afterwards would be
    /// repeating what the first said.
    pub fn joining(&self, joined: Joined) {
        let _ = self.joined.set(joined);
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
        operator.implementations.clone_from(&self.implementations);
        operator.reference_impl = self.implementations.iter().all(|chosen| chosen.is_reference);
        operator.rows_in = self.rows_in.load(Ordering::Relaxed);
        operator.rows_out = self.rows_out.load(Ordering::Relaxed);
        operator.wall_ns = self.wall_ns.load(Ordering::Relaxed);
        operator.cpu_ns = self.cpu_ns.load(Ordering::Relaxed);
        operator.bytes_read = self.bytes_read.load(Ordering::Relaxed);
        operator.bytes_decoded = self.bytes_decoded.load(Ordering::Relaxed);
        operator.bytes_spilled = self.bytes_spilled.load(Ordering::Relaxed);
        operator.parts_read = self.parts_read.load(Ordering::Relaxed);
        operator.parts_pruned = self.parts_pruned.load(Ordering::Relaxed);
        for cause in Cause::ALL {
            let seen = self.fallbacks[cause.slot()].load(Ordering::Relaxed);
            operator.fallbacks.add(Tally::of(cause, seen));
        }
        for stage in Stage::ALL {
            let nanos = self.stages[stage.slot()].load(Ordering::Relaxed);
            let bytes = self.stage_bytes[stage.slot()].load(Ordering::Relaxed);
            operator.stages.add(Spent::of(stage, nanos, bytes));
        }
        operator.memory = Memory {
            reserved: self.reserved.load(Ordering::Relaxed),
            high_water: self.high_water.load(Ordering::Relaxed),
        };
        operator.joined = self.joined.get().cloned();
        operator
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;
    use std::thread;

    use rudb_common::{Cause, Spent, Stage, Tally};

    use super::Counters;
    use crate::document::{Algorithm, Declined, Joined};

    #[test]
    fn falling_back_adds_up_across_the_calls_and_comes_out_split_by_cause() {
        let counters = Counters::new(0, 0, "Filter");
        counters.fell_back(Tally::of(Cause::Compare, 3));
        counters.fell_back(Tally::none());
        let mut both = Tally::of(Cause::Compare, 1);
        both.add(Tally::of(Cause::Flatten, 10));
        counters.fell_back(both);
        let operator = counters.snapshot();
        assert_eq!(operator.fallbacks.get(Cause::Compare), 4);
        assert_eq!(operator.fallbacks.get(Cause::Flatten), 10);
        assert_eq!(operator.fallbacks.get(Cause::Cast), 0);
        assert_eq!(operator.fallbacks.total(), 14);
        assert_eq!(operator.fallbacks.worst(), Some((Cause::Flatten, 10)));
    }

    #[test]
    fn an_operator_that_never_gave_up_reports_nothing_rather_than_a_row_of_zeroes() {
        assert!(Counters::new(0, 0, "Scan").snapshot().fallbacks.is_empty());
    }

    #[test]
    fn the_stages_of_a_read_add_up_across_the_calls_and_come_out_split() {
        let counters = Counters::new(0, 0, "FileScan");
        counters.spent_reading(Spent::of(Stage::Read, 400, 65_536));
        counters.spent_reading(Spent::none());
        let mut both = Spent::of(Stage::Read, 100, 16_384);
        both.add(Spent::of(Stage::Decompress, 9_000, 262_144));
        both.add(Spent::of(Stage::Decode, 7_000, 245_760));
        both.add(Spent::of(Stage::Dictionary, 500, 16_384));
        counters.spent_reading(both);
        let operator = counters.snapshot();
        assert_eq!(operator.stages.nanos(Stage::Read), 500);
        assert_eq!(operator.stages.bytes(Stage::Read), 81_920);
        assert_eq!(operator.stages.nanos(Stage::Decompress), 9_000);
        assert_eq!(operator.stages.nanos(Stage::Decode), 7_000);
        assert_eq!(operator.bytes_decoded, 262_144);
        assert_eq!(operator.stages.total(), 17_000);
        // The whole point of the split, which is that the answer is a stage rather than a scan.
        assert_eq!(operator.stages.worst(), Some((Stage::Decompress, 9_000)));
    }

    #[test]
    fn an_operator_that_reads_nothing_keeps_the_stages_out_of_the_document() {
        assert!(Counters::new(0, 0, "Filter").snapshot().stages.is_empty());
    }

    #[test]
    fn a_snapshot_carries_what_was_counted_and_what_was_known() {
        let counters = Counters::new(3, 0, "Scan").detailed("hits").estimated(1000).chose(
            "vector.form",
            "flat",
            true,
        );
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
    fn an_operator_that_chose_something_faster_is_not_marked_as_a_reference() {
        // The case the old flag could not express, and the reason it was worth replacing. One seam
        // picked the fast implementation, so this operator's number is a number worth quoting even
        // though the seam beside it did not.
        let counters = Counters::new(1, 0, "Filter")
            .chose("chunk.compaction", "gain", false)
            .chose("expr.eval", "tree", true);
        let operator = counters.snapshot();
        assert!(!operator.reference_impl);
        assert_eq!(operator.implementations.len(), 2);
        assert_eq!(operator.implementations[0].seam, "chunk.compaction");
        assert_eq!(operator.implementations[0].name, "gain");
    }

    #[test]
    fn an_operator_that_had_nothing_to_choose_from_is_still_a_reference() {
        // A limit sits on no seam and there is one way to count to ten, so the marker stays on and
        // the list stays empty. Empty means there was nothing to choose, not that nothing ran.
        let operator = Counters::new(1, 0, "Limit").snapshot();
        assert!(operator.reference_impl);
        assert!(operator.implementations.is_empty());
    }

    #[test]
    fn the_instance_that_built_the_table_is_the_one_that_says_what_the_join_did() {
        let counters = Counters::new(0, 0, "Probe");
        assert!(
            counters.snapshot().joined.is_none(),
            "an operator that is not a join says nothing"
        );
        counters.joining(Joined {
            algorithm: Algorithm::Hash,
            build_rows: 300,
            build_bytes: 18_128,
            declined: vec![Declined::new(Algorithm::Loop, "the condition holds an equality")],
        });
        // Every instance of the operator runs the same code and the ones that arrive after the
        // table is built have nothing to add, so a second report is dropped rather than averaged
        // or summed into a number that is neither of the two.
        counters.joining(Joined {
            algorithm: Algorithm::Loop,
            build_rows: 9,
            build_bytes: 9,
            declined: Vec::new(),
        });
        let joined = counters.snapshot().joined.expect("a join reported");
        assert_eq!(joined.algorithm, Algorithm::Hash);
        assert_eq!(joined.build_rows, 300);
        assert_eq!(joined.build_bytes, 18_128);
        assert_eq!(joined.declined.len(), 1);
        assert_eq!(joined.declined[0].algorithm, Algorithm::Loop);
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
