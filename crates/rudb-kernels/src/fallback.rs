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

use std::sync::atomic::{AtomicU64, Ordering};

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

static COUNTS: [AtomicU64; CELLS] = [const { AtomicU64::new(0) }; CELLS];

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
    COUNTS[cell(kernel, left, right)].fetch_add(1, Ordering::Relaxed);
}

/// How many times a kernel fell through on this pair of forms.
#[must_use]
pub fn count(kernel: Kernel, left: Form, right: Form) -> u64 {
    COUNTS[cell(kernel, left, right)].load(Ordering::Relaxed)
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
    for counter in &COUNTS {
        counter.store(0, Ordering::Relaxed);
    }
}

/// The lock every test that resets the counters holds while it does.
///
/// The counters are process wide and the test harness runs tests in parallel, so two tests that
/// both reset would otherwise pass alone and fail together, which is the worst kind of test to own.
/// It lives here rather than in the test module below because the kernel tests in the other files
/// reset the counters too and they need the same lock, not a second one.
#[cfg(test)]
pub(crate) static TURN: std::sync::Mutex<()> = std::sync::Mutex::new(());

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
    use super::{Form, Kernel, TURN, count, hot, record, report, reset};

    #[test]
    fn a_fall_through_lands_in_the_cell_for_its_own_form_pair() {
        let _turn = TURN.lock().expect("no test panics while holding this");
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
        let _turn = TURN.lock().expect("no test panics while holding this");
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
