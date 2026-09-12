//! How often a kernel took the row at a time path, and for which shape of input.
//!
//! `spec/engine/03-data-plane.md` asks for this by name, and the reason is that the alternative to
//! counting is guessing. There are four physical forms, so sixteen form pairs per kernel, and
//! writing a hand tuned loop for all sixteen is both a lot of code and a lot of places for a wrong
//! answer to hide. Writing three of them and a correct slow path for the rest is the right amount
//! of code, but only if there is a way to find out that the fourth is on the hot path of a real
//! query. That way is this.
//!
//! What gets counted is the fall through, not the fast path. A counter on the fast path would cost
//! an atomic increment per vector on the loop this whole layer exists to make fast, and it would
//! measure something nobody needs to know. A counter on the slow path costs an atomic increment on
//! a loop that is already allocating a `Value` per row, which is not measurable next to what it
//! sits on.
//!
//! The counts are process wide and never reset by the library. A benchmark harness reads them at
//! the end of a run and prints the ones that are not zero, which turns "we should probably
//! specialize sequence against constant" into either a number or silence.
//!
//! There is a second counter and [`record`] bumps both. This one answers which form pair to go and
//! write a specialization for, which is a question about a build rather than about a query, so it is
//! process wide and has no idea which operator was running. [`rudb_common::slow`] answers which
//! operator in this query is the one paying, which needs the count to be per thread so that the
//! instrumentation shim can take a difference around a call. Neither number can be worked out from
//! the other, they cost an add each, and the alternative to having both is reading one of them and
//! guessing the other.

use std::sync::atomic::{AtomicU64, Ordering};

use rudb_common::{Cause, slow};
use rudb_vector::Form;

/// Which kernel fell through.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Kernel {
    /// The comparisons, in `compare`.
    Compare,
    /// The scalar functions, in `scalar`.
    Scalar,
    /// Three-valued logic, in `logic`.
    Logic,
    /// The conversions, in `cast`.
    Cast,
    /// The aggregates.
    Aggregate,
    /// Turning a vector of flags into the rows it keeps, in `select`.
    Select,
}

impl Kernel {
    /// Every kernel that reports, in the order the table prints them.
    const ALL: [Self; 6] =
        [Self::Compare, Self::Scalar, Self::Logic, Self::Cast, Self::Aggregate, Self::Select];

    /// The name used in the report.
    #[must_use]
    pub fn name(self) -> &'static str {
        match self {
            Self::Compare => "compare",
            Self::Scalar => "scalar",
            Self::Logic => "logic",
            Self::Cast => "cast",
            Self::Aggregate => "aggregate",
            Self::Select => "select",
        }
    }

    fn index(self) -> usize {
        match self {
            Self::Compare => 0,
            Self::Scalar => 1,
            Self::Logic => 2,
            Self::Cast => 3,
            Self::Aggregate => 4,
            Self::Select => 5,
        }
    }

    /// The same kernel as the metrics document names it.
    ///
    /// Two enums for one list of kernels is not ideal and it is the layer rule rather than a
    /// preference. The document is written at rank 4 and this crate is at rank 3, so the vocabulary
    /// the document is spelled in has to be somewhere both can see, which is rank 0. The test below
    /// is what keeps the two lists the same list.
    const fn cause(self) -> Cause {
        match self {
            Self::Compare => Cause::Compare,
            Self::Scalar => Cause::Scalar,
            Self::Logic => Cause::Logic,
            Self::Cast => Cause::Cast,
            Self::Aggregate => Cause::Aggregate,
            Self::Select => Cause::Select,
        }
    }
}

/// Every physical form, in the order the table prints them.
const FORMS: [Form; 4] = [Form::Flat, Form::Constant, Form::Sequence, Form::Dictionary];

/// The name of a form, for the report.
fn form_name(form: Form) -> &'static str {
    match form {
        Form::Flat => "flat",
        Form::Constant => "constant",
        Form::Sequence => "sequence",
        Form::Dictionary => "dictionary",
        // `Form` is not exhaustive as far as this crate is concerned, and layer three adds
        // `Encoded` to it. A name rather than a panic means the day that lands is a day the report
        // says `other` for a while, not a day the report aborts the process.
        _ => "other",
    }
}

/// The position of a form in [`FORMS`], or four for one this build does not know about.
fn form_index(form: Form) -> usize {
    FORMS.iter().position(|&known| known == form).unwrap_or(FORMS.len())
}

/// One counter per kernel per form pair, plus a row and a column for a form added later.
const WIDTH: usize = FORMS.len() + 1;
const CELLS: usize = Kernel::ALL.len() * WIDTH * WIDTH;

#[cfg(not(test))]
static COUNTS: [AtomicU64; CELLS] = [const { AtomicU64::new(0) }; CELLS];

// One table per thread in a test build, and one table for the process everywhere else.
//
// The counts a harness wants are the counts for a run, so the table the library keeps is process
// wide. The counts a test wants are its own, and the test harness runs tests in parallel in one
// process, so under `cfg(test)` every thread gets a table of its own and a test sees nothing but
// what it recorded. A test binary here is a hundred and twenty tests of which fourteen read these
// counters and the rest call kernels, so with one shared table the fourteen fail whenever one of
// the other hundred happens to fall through at the same moment. That is what took the 0.2.12
// release down and it did it by failing on a machine nobody was watching.
//
// A lock is the other way to write this and it was the way this was written. It does not work,
// because it only serializes the tests that take it, and the test that has to take it is every
// test that calls a kernel rather than the ones that read the counters.
#[cfg(test)]
thread_local! {
    static COUNTS: [AtomicU64; CELLS] = const { [const { AtomicU64::new(0) }; CELLS] };
}

/// Reads the table this thread counts into.
#[cfg(not(test))]
fn with_counts<T>(read: impl FnOnce(&[AtomicU64; CELLS]) -> T) -> T {
    read(&COUNTS)
}

/// Reads the table this thread counts into.
#[cfg(test)]
fn with_counts<T>(read: impl FnOnce(&[AtomicU64; CELLS]) -> T) -> T {
    COUNTS.with(read)
}

/// Where a kernel and a form pair live in the table.
fn cell(kernel: Kernel, left: Form, right: Form) -> usize {
    kernel.index() * WIDTH * WIDTH + form_index(left) * WIDTH + form_index(right)
}

/// Records that a kernel took the row at a time path on this pair of forms.
///
/// `Relaxed` because nothing reads this to make a decision while a query is running. It is a
/// diagnostic that is read once, after, by a harness, and paying for ordering on it would be
/// paying for a guarantee nobody uses.
pub fn record(kernel: Kernel, left: Form, right: Form) {
    with_counts(|counts| counts[cell(kernel, left, right)].fetch_add(1, Ordering::Relaxed));
    slow::took(kernel.cause());
}

/// How many times a kernel fell through on this pair of forms.
#[must_use]
pub fn count(kernel: Kernel, left: Form, right: Form) -> u64 {
    with_counts(|counts| counts[cell(kernel, left, right)].load(Ordering::Relaxed))
}

/// Every combination that has fallen through at least once, most frequent first.
#[must_use]
pub fn hot() -> Vec<(Kernel, Form, Form, u64)> {
    let mut out = Vec::new();
    for kernel in Kernel::ALL {
        for left in FORMS {
            for right in FORMS {
                let seen = count(kernel, left, right);
                if seen > 0 {
                    out.push((kernel, left, right, seen));
                }
            }
        }
    }
    out.sort_by_key(|entry| std::cmp::Reverse(entry.3));
    out
}

/// Sets every counter back to zero.
///
/// For a harness that wants the counts for one query rather than for a process, and for the tests
/// below. It is not synchronized against a running query, because a diagnostic that took a lock
/// would be a diagnostic that changed what it measures.
pub fn reset() {
    with_counts(|counts| {
        for counter in counts {
            counter.store(0, Ordering::Relaxed);
        }
    });
}

/// The counts as a table, or a line saying there are none.
#[must_use]
pub fn report() -> String {
    let hot = hot();
    if hot.is_empty() {
        return "every kernel call took a specialized path".to_owned();
    }
    let mut out = String::from("kernel calls that fell through to the row at a time path\n");
    for (kernel, left, right, seen) in hot {
        out.push_str(&format!(
            "  {:<10} {:<10} against {:<10} {seen}\n",
            kernel.name(),
            form_name(left),
            form_name(right)
        ));
    }
    out
}

#[cfg(test)]
mod tests {
    use rudb_common::slow;

    use super::{Cause, Form, Kernel, count, hot, record, report, reset};

    #[test]
    fn a_fall_through_is_counted_by_form_pair_here_and_by_kernel_where_the_document_reads_it() {
        reset();
        slow::reset();
        record(Kernel::Select, Form::Dictionary, Form::Flat);
        record(Kernel::Select, Form::Constant, Form::Flat);
        assert_eq!(count(Kernel::Select, Form::Dictionary, Form::Flat), 1);
        assert_eq!(count(Kernel::Select, Form::Constant, Form::Flat), 1);
        // The other counter does not split by form, because the question it answers is which
        // operator is paying rather than which specialization is missing.
        assert_eq!(slow::here().get(Cause::Select), 2);
        assert_eq!(slow::here().total(), 2);
        reset();
        slow::reset();
    }

    #[test]
    fn every_kernel_names_a_cause_of_its_own() {
        let mut named: Vec<&str> = Kernel::ALL.iter().map(|kernel| kernel.cause().name()).collect();
        named.sort_unstable();
        named.dedup();
        assert_eq!(named.len(), Kernel::ALL.len());
        for kernel in Kernel::ALL {
            assert_eq!(kernel.name(), kernel.cause().name(), "one kernel, one name");
        }
    }

    #[test]
    fn a_fall_through_lands_in_the_cell_for_its_own_form_pair() {
        reset();
        record(Kernel::Compare, Form::Sequence, Form::Constant);
        record(Kernel::Compare, Form::Sequence, Form::Constant);
        record(Kernel::Cast, Form::Dictionary, Form::Flat);
        assert_eq!(count(Kernel::Compare, Form::Sequence, Form::Constant), 2);
        assert_eq!(count(Kernel::Cast, Form::Dictionary, Form::Flat), 1);
        assert_eq!(count(Kernel::Compare, Form::Constant, Form::Sequence), 0);
        assert_eq!(count(Kernel::Compare, Form::Flat, Form::Flat), 0);
        reset();
    }

    #[test]
    fn the_report_names_the_combination_rather_than_a_number_on_its_own() {
        reset();
        assert!(report().contains("every kernel call took a specialized path"));
        for _ in 0..7 {
            record(Kernel::Compare, Form::Sequence, Form::Constant);
        }
        record(Kernel::Logic, Form::Flat, Form::Dictionary);
        let text = report();
        assert!(text.contains("compare"), "{text}");
        assert!(text.contains("sequence"), "{text}");
        assert!(text.contains('7'), "{text}");
        // Most frequent first, because the point of the table is to say what to specialize next.
        assert_eq!(hot().first().map(|entry| entry.3), Some(7));
        reset();
    }
}
