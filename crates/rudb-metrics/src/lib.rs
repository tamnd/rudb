//! The metrics document one execution produces, and the JSON it is written as.
//!
//! Rank 1 in the layer rule. See `xtask/layers.toml` and `spec/18-package-layout.md`.
//!
//! Every execution produces one of these, whether it succeeded, was cancelled or failed. A failed
//! query's metrics are the most useful metrics there are, because the question after a failure is
//! always what it had done by the time it stopped, and a document that is only written on success
//! cannot answer it.
//!
//! # What is in it
//!
//! The query, the engine and machine it ran on, the settings it ran under, how it ended, the
//! timings, the resources it used, the strategy chosen at every seam, one row per pipeline, one row
//! per operator, and a list of warnings. [`Document::render`] writes it as JSON and that JSON is
//! what `rudb-bench` reads, what `--metrics run.json` writes and what `EXPLAIN ANALYZE` is a
//! rendering of.
//!
//! # Four things about the shape
//!
//! It is versioned. [`SCHEMA`] is the number in the document, and `rudb-bench` reads documents from
//! every commit in the ledger, so a reader has to know what it is holding. Adding a field is not a
//! schema change, because a reader that does not know a field ignores it and a reader that wants
//! one it cannot find has the same problem it had before the field existed. Changing what a field
//! means, or removing one, is a schema change and it is a migration with a test.
//!
//! Lists are flat with a parent id on each row, rather than a tree. A pipeline names the pipelines
//! it depends on and an operator names its pipeline. That is easier to query than nesting, and it
//! is what lets `SELECT * FROM rudb_metrics()` be a table rather than a document walk. The
//! strategies are a list for the same reason, which is the one place this differs from
//! `spec/engine/13-measurement.md`'s sketch: a map keyed by seam has to invent a column name for
//! its key before it can be a row, and every other list here already has one.
//!
//! Strategies are first class. Without the strategy at each seam a ledger row is not reproducible,
//! and reproducibility is the whole reason the seams exist.
//!
//! Warnings are generated rather than written. Anything the engine knows it did badly, which is a
//! spill, a reference implementation, a row at a time path, an estimate off by an order of
//! magnitude, time spent blocked or CPU that no operator accounts for, becomes a line in
//! [`Document::warnings`] from the numbers themselves. Nothing calls a warn function, so nothing
//! can forget to. The warnings list is what somebody reads first.
//!
//! # What fills it in
//!
//! [`Counters`] is what one operator counts into while it runs, and [`Span`] is the pair of clock
//! readings that one call costs. Both are here rather than next to the operators because the rank
//! is 1: the shim that wraps every operator lives in `rudb-pipeline` at rank 4 and reports into
//! this, and so will the buffer manager and the file readers, none of which can see each other.
//!
//! [`Counters::snapshot`] is the only way a counter becomes a row, so an operator that was measured
//! and an operator that was written by hand into a test produce the same shape.
//!
//! One of the things [`Counters`] holds is not counted here at all. [`rudb_common::slow`] is a per
//! thread count of every path the engine took that was written to be correct rather than fast, a
//! kernel that met a pair of forms nobody specialized or a compact column that got copied out flat,
//! and the shim reads it before an operator call and after it so that the difference lands against
//! the operator that did it. That count is the F1 work list, and the reason it is at rank 0 rather
//! than in this crate is that the two things that increment it are at rank 1 and rank 3 and cannot
//! see each other or this.
//!
//! [`Driver`] is the same idea one level up. The loop that runs a pipeline is not an operator and
//! nothing else measures it, so a chunk that costs four operator calls also costs a trip round a
//! loop that allocated the chunk and dropped it, and on a query that moves ten thousand chunks that
//! adds up to a third of the execution. A pipeline's time is its driver's time, and the `driver`
//! module explains how pipelines that run inside each other avoid counting the same time twice.
//!
//! [`Report`] is the other end of all those counters. Whoever builds an execution registers each
//! operator with one as it is made and says which pipeline depends on which, and at the end
//! [`Report::fill`] puts the rows into the document. That is the piece that makes the ids and the
//! pipeline numbers come from the shape of the plan rather than from anything an operator says
//! about itself.
//!
//! # The one unsafe block
//!
//! Per thread CPU time is a system call and there is no dependency here to make it for us, so
//! `clock` declares `clock_gettime` and calls it. That is the whole of the unsafe in this crate, it
//! is compiled only on the two operating systems whose `timespec` the declaration matches, and
//! everywhere else the clock reports nothing rather than a guess.

mod clock;
mod counters;
mod document;
mod driver;
mod json;
mod report;
mod warn;

pub use clock::{Span, thread_cpu_ns};
pub use counters::Counters;
pub use document::{
    Blocked, Document, Engine, Machine, Memory, Operator, Outcome, Pipeline, Query, Resource,
    Settings, Strategy, Timing,
};
pub use driver::{Driver, Running};
pub use report::Report;

/// The version of the document this crate writes.
///
/// See the crate documentation for what a change to it means and what does not need one.
pub const SCHEMA: u32 = 1;

/// A number with thousands separators, the way a report prints a row count.
///
/// Ninety nine million rows is a number a reader has to count the digits of, and `99,997,497` is
/// one they can read. This is here rather than in the warning code because `EXPLAIN ANALYZE` prints
/// the same counts out of the same document.
#[must_use]
pub fn commas(value: u64) -> String {
    let digits = value.to_string();
    let mut out = String::with_capacity(digits.len() + digits.len() / 3);
    for (at, digit) in digits.chars().enumerate() {
        if at > 0 && (digits.len() - at) % 3 == 0 {
            out.push(',');
        }
        out.push(digit);
    }
    out
}

/// A span of nanoseconds, in whichever unit makes it readable.
///
/// `1.323s`, `620ms`, `41us`, `900ns`. The arithmetic is integer arithmetic, because a duration in
/// a report is a label and two machines that agree on the nanoseconds should agree on the label.
#[must_use]
pub fn duration(ns: u64) -> String {
    if ns < 1_000 {
        format!("{ns}ns")
    } else if ns < 1_000_000 {
        format!("{}us", ns / 1_000)
    } else if ns < 1_000_000_000 {
        format!("{}ms", ns / 1_000_000)
    } else {
        format!("{}.{:03}s", ns / 1_000_000_000, ns % 1_000_000_000 / 1_000_000)
    }
}

#[cfg(test)]
mod tests {
    use super::{commas, duration};

    #[test]
    fn a_span_is_printed_in_the_unit_that_reads() {
        assert_eq!(duration(0), "0ns");
        assert_eq!(duration(999), "999ns");
        assert_eq!(duration(41_000), "41us");
        assert_eq!(duration(620_000_000), "620ms");
        assert_eq!(duration(1_323_483_000), "1.323s");
    }

    #[test]
    fn a_number_is_grouped_from_the_right() {
        assert_eq!(commas(0), "0");
        assert_eq!(commas(999), "999");
        assert_eq!(commas(1000), "1,000");
        assert_eq!(commas(99_997_497), "99,997,497");
        assert_eq!(commas(u64::MAX), "18,446,744,073,709,551,615");
    }
}
