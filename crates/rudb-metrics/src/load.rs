//! The load profile: what each stage of a bulk load cost.
//!
//! `engine-v4/16-measurement.md` section 16.2 asks for one row per stage of the bulk path and one
//! column per measure, for every statement that writes a table in bulk. Before this a load reported
//! one wall clock number, and a number that says the whole load took 579 s says nothing about which
//! stage to work on. The finding in `spec/perf/10-what-the-encoder-costs.md` that four codecs took
//! 62.9% of encode time was made by hand. A profile makes that kind of finding the default output.
//!
//! # The stages
//!
//! [`Stage`] names the nine of `04-the-bulk-path.md` section 4.1. The writer at 0.4 has five of
//! them as separate code: conversion happens upstream in the scan, then the page builder, the
//! dictionary blocks, the page writes and the publish at the end. Split, the structural index,
//! transcoding and the extent allocator do not exist as stages yet, so nothing charges them and
//! `rudb_write_metrics()` leaves them out rather than print a row of zeros that reads as "free".
//! They start reporting when W1 and W2 build them.
//!
//! Publish at 0.4 is more than the commit. It is everything a table needs once its last stripe is
//! down: the exact heavy hitters of every numeric column, which reads the pages back, the table's
//! statistics and directory, the catalog and the two syncs. The heavy hitter count runs on threads
//! of its own and each of them charges its own span, so its CPU time is in the row.
//!
//! # What it costs
//!
//! Every counter is a relaxed atomic added once per unit of work, which is a worker, a stripe or
//! a statement and never a row or a chunk. The thread CPU clock is a system call on Linux (see
//! [`crate::Span`]), so it is read at those same boundaries and nowhere else. A load of `hits` is
//! about a hundred stripes, which puts the whole profile at a few thousand atomic adds and a few
//! hundred clock reads for a statement that runs for minutes.
//!
//! Wall time is summed over workers, as section 4.13 says. A stage that ran on eight threads for a
//! second reports eight seconds, which makes the stage rows comparable with each other and with CPU
//! time, and the `total` row is the one that says how long the statement took.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Instant;

use crate::Span;

/// How many finished and running loads a process keeps for `rudb_write_metrics()`.
pub const KEPT_LOADS: usize = 16;

/// One stage of the bulk path, in the order of `04-the-bulk-path.md` section 4.1.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Stage {
    /// Cutting the source into pieces workers can take.
    Split,
    /// Finding the field and record boundaries of a text source.
    Index,
    /// Turning source values into vectors. For a Parquet source it is the scan: reading,
    /// decompressing and decoding row groups, and every operator between the scan and the writer.
    Convert,
    /// Encoding vectors into pages, with the per column statistics folded on the same pass.
    Pages,
    /// Building the global dictionaries and writing their blocks.
    Dictionary,
    /// Rewriting source dictionary codes into global ones.
    Transcode,
    /// Handing out file space to workers that write in parallel.
    Extents,
    /// Writing the pages of a stripe and its indexes.
    Write,
    /// Committing the directory and making the file visible.
    Publish,
}

impl Stage {
    /// Every stage, in pipeline order.
    pub const ALL: [Stage; 9] = [
        Self::Split,
        Self::Index,
        Self::Convert,
        Self::Pages,
        Self::Dictionary,
        Self::Transcode,
        Self::Extents,
        Self::Write,
        Self::Publish,
    ];

    /// The name the spec and `rudb_write_metrics()` use.
    #[must_use]
    pub const fn name(self) -> &'static str {
        match self {
            Self::Split => "split",
            Self::Index => "structural index",
            Self::Convert => "convert",
            Self::Pages => "page builder",
            Self::Dictionary => "dictionary",
            Self::Transcode => "transcode",
            Self::Extents => "extent allocator",
            Self::Write => "write",
            Self::Publish => "publish",
        }
    }

    const fn slot(self) -> usize {
        self as usize
    }
}

/// One stage's counters.
#[derive(Debug, Default)]
struct Counters {
    charged: AtomicU64,
    wall_ns: AtomicU64,
    cpu_ns: AtomicU64,
    bytes_in: AtomicU64,
    bytes_out: AtomicU64,
    rows: AtomicU64,
    waits: AtomicU64,
    wait_ns: AtomicU64,
}

/// What one stage had cost when it was read.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct StageTotals {
    /// How many times something was charged to it. Zero means the stage did not run.
    pub charged: u64,
    /// Wall time summed over the workers that ran it.
    pub wall_ns: u64,
    /// Thread CPU time summed over the same workers.
    pub cpu_ns: u64,
    /// Bytes the stage was handed.
    pub bytes_in: u64,
    /// Bytes it produced. For the stages that write, bytes it put in the file.
    pub bytes_out: u64,
    /// Rows that went through it.
    pub rows: u64,
    /// How many times a worker waited on something before it could do this stage's work.
    pub waits: u64,
    /// How long those waits took, summed.
    pub wait_ns: u64,
}

/// The profile of one bulk-path statement.
#[derive(Debug)]
pub struct LoadProfile {
    id: u64,
    target: String,
    started: Instant,
    finished_ns: AtomicU64,
    stages: [Counters; 9],
}

impl LoadProfile {
    /// Starts the profile of a load into `target`, and keeps it where `rudb_write_metrics()` can
    /// find it.
    ///
    /// The oldest kept profile is dropped once there are more than [`KEPT_LOADS`], so a process
    /// that loads all day holds a bounded amount. A profile somebody still holds a handle to lives
    /// on until they let it go, which is how the writer of a long load keeps charging a profile
    /// that sixteen quick loads have pushed out of the list.
    #[must_use]
    pub fn begin(target: impl Into<String>) -> Arc<Self> {
        let mut kept = KEPT.lock().unwrap_or_else(PoisonError::into_inner);
        kept.next = kept.next.saturating_add(1);
        let profile = Arc::new(Self {
            id: kept.next,
            target: target.into(),
            started: Instant::now(),
            finished_ns: AtomicU64::new(0),
            stages: Default::default(),
        });
        kept.loads.push(Arc::clone(&profile));
        if kept.loads.len() > KEPT_LOADS {
            kept.loads.remove(0);
        }
        profile
    }

    /// The number this process gave the load, counting from one.
    #[must_use]
    pub fn id(&self) -> u64 {
        self.id
    }

    /// The table being written.
    #[must_use]
    pub fn target(&self) -> &str {
        &self.target
    }

    /// Adds time to a stage.
    pub fn charge(&self, stage: Stage, wall_ns: u64, cpu_ns: u64) {
        let counters = &self.stages[stage.slot()];
        counters.charged.fetch_add(1, Ordering::Relaxed);
        counters.wall_ns.fetch_add(wall_ns, Ordering::Relaxed);
        counters.cpu_ns.fetch_add(cpu_ns, Ordering::Relaxed);
    }

    /// Adds what a stage was handed and what it produced.
    pub fn moved(&self, stage: Stage, bytes_in: u64, bytes_out: u64, rows: u64) {
        let counters = &self.stages[stage.slot()];
        counters.bytes_in.fetch_add(bytes_in, Ordering::Relaxed);
        counters.bytes_out.fetch_add(bytes_out, Ordering::Relaxed);
        counters.rows.fetch_add(rows, Ordering::Relaxed);
    }

    /// Adds one wait before a stage's work.
    pub fn waited(&self, stage: Stage, wait_ns: u64) {
        let counters = &self.stages[stage.slot()];
        counters.waits.fetch_add(1, Ordering::Relaxed);
        counters.wait_ns.fetch_add(wait_ns, Ordering::Relaxed);
    }

    /// Times whatever runs until the returned guard is dropped, on both clocks, and charges it to
    /// `stage`.
    ///
    /// Both clocks, so this is for a boundary that is crossed once per stripe or once per worker.
    /// Around anything per chunk the thread clock would cost more than the work.
    #[must_use]
    pub fn span(&self, stage: Stage) -> StageSpan<'_> {
        StageSpan { profile: self, stage, span: Some(Span::start()) }
    }

    /// Marks the statement finished, which fixes the `total` row's wall time.
    ///
    /// The first call wins, so whoever drops the load after it committed does not move the time
    /// the commit fixed.
    pub fn finish(&self) {
        let elapsed = u64::try_from(self.started.elapsed().as_nanos()).unwrap_or(u64::MAX);
        let _first = self.finished_ns.compare_exchange(
            0,
            elapsed.max(1),
            Ordering::Relaxed,
            Ordering::Relaxed,
        );
    }

    /// The statement's wall time: how long it took if it finished, and how long it has been going
    /// if it has not.
    #[must_use]
    pub fn elapsed_ns(&self) -> u64 {
        match self.finished_ns.load(Ordering::Relaxed) {
            0 => u64::try_from(self.started.elapsed().as_nanos()).unwrap_or(u64::MAX),
            done => done,
        }
    }

    /// Whether [`Self::finish`] has been called.
    #[must_use]
    pub fn finished(&self) -> bool {
        self.finished_ns.load(Ordering::Relaxed) != 0
    }

    /// What one stage has cost so far.
    #[must_use]
    pub fn stage(&self, stage: Stage) -> StageTotals {
        let counters = &self.stages[stage.slot()];
        StageTotals {
            charged: counters.charged.load(Ordering::Relaxed),
            wall_ns: counters.wall_ns.load(Ordering::Relaxed),
            cpu_ns: counters.cpu_ns.load(Ordering::Relaxed),
            bytes_in: counters.bytes_in.load(Ordering::Relaxed),
            bytes_out: counters.bytes_out.load(Ordering::Relaxed),
            rows: counters.rows.load(Ordering::Relaxed),
            waits: counters.waits.load(Ordering::Relaxed),
            wait_ns: counters.wait_ns.load(Ordering::Relaxed),
        }
    }
}

/// A stage being timed, which charges itself when it is dropped.
#[derive(Debug)]
pub struct StageSpan<'a> {
    profile: &'a LoadProfile,
    stage: Stage,
    span: Option<Span>,
}

impl Drop for StageSpan<'_> {
    fn drop(&mut self) {
        if let Some(span) = self.span.take() {
            let (wall, cpu) = span.stop();
            self.profile.charge(self.stage, wall, cpu);
        }
    }
}

#[derive(Debug)]
struct Kept {
    next: u64,
    loads: Vec<Arc<LoadProfile>>,
}

static KEPT: Mutex<Kept> = Mutex::new(Kept { next: 0, loads: Vec::new() });

/// The loads this process has kept, oldest first.
#[must_use]
pub fn recent_loads() -> Vec<Arc<LoadProfile>> {
    KEPT.lock().unwrap_or_else(PoisonError::into_inner).loads.clone()
}

#[cfg(test)]
mod tests {
    use super::{KEPT_LOADS, LoadProfile, Stage, recent_loads};

    #[test]
    fn a_stage_sums_what_every_worker_charged() {
        let profile = LoadProfile::begin("t");
        std::thread::scope(|scope| {
            for _ in 0..4 {
                scope.spawn(|| {
                    profile.charge(Stage::Pages, 10, 7);
                    profile.moved(Stage::Pages, 100, 40, 5);
                });
            }
        });
        profile.waited(Stage::Write, 3);
        let pages = profile.stage(Stage::Pages);
        assert_eq!((pages.charged, pages.wall_ns, pages.cpu_ns), (4, 40, 28));
        assert_eq!((pages.bytes_in, pages.bytes_out, pages.rows), (400, 160, 20));
        let write = profile.stage(Stage::Write);
        assert_eq!((write.charged, write.waits, write.wait_ns), (0, 1, 3));
        assert_eq!(profile.stage(Stage::Split).charged, 0);
    }

    #[test]
    fn a_span_charges_once_when_it_is_dropped() {
        let profile = LoadProfile::begin("t");
        {
            let _span = profile.span(Stage::Publish);
            std::hint::black_box((0..10_000).sum::<u64>());
        }
        let publish = profile.stage(Stage::Publish);
        assert_eq!(publish.charged, 1);
        assert!(publish.wall_ns > 0);
    }

    #[test]
    fn the_total_stops_at_finish() {
        let profile = LoadProfile::begin("t");
        assert!(!profile.finished());
        profile.finish();
        let done = profile.elapsed_ns();
        std::thread::sleep(std::time::Duration::from_millis(2));
        profile.finish();
        assert!(profile.finished());
        assert_eq!(profile.elapsed_ns(), done);
    }

    #[test]
    fn the_process_keeps_the_newest_loads() {
        let first = LoadProfile::begin("first");
        let mut last = None;
        for at in 0..=KEPT_LOADS {
            last = Some(LoadProfile::begin(format!("later {at}")));
        }
        let kept = recent_loads();
        assert!(kept.len() <= KEPT_LOADS);
        assert!(kept.iter().all(|load| load.id() != first.id()));
        let last = last.expect("the loop ran");
        assert!(kept.iter().any(|load| load.id() == last.id()));
        assert!(kept.windows(2).all(|pair| pair[0].id() < pair[1].id()));
        // Dropped from the list, and still usable by whoever holds it.
        first.charge(Stage::Convert, 1, 1);
        assert_eq!(first.stage(Stage::Convert).charged, 1);
    }

    #[test]
    fn the_stage_names_are_the_specs() {
        let names: Vec<&str> = Stage::ALL.iter().map(|stage| stage.name()).collect();
        assert_eq!(
            names,
            [
                "split",
                "structural index",
                "convert",
                "page builder",
                "dictionary",
                "transcode",
                "extent allocator",
                "write",
                "publish"
            ]
        );
    }
}
