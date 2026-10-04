//! The statements a process ran lately and what each phase of them cost, for
//! `rudb_statement_metrics()`.
//!
//! Milestone C0 of `spec/compiler/18-milestones.md` asks for the frontend of every statement to be
//! timed phase by phase, parse, bind, rewrite and optimize, and for the numbers to be readable from
//! SQL. The timing was already there: the statement path times parse, bind and the optimizer on the
//! way to the metrics document, and `EXPLAIN ANALYZE` and `--metrics` print them. What was missing
//! was a way to read them for a statement somebody had already run without knowing in advance that
//! they would want to. This is that way. Every statement leaves a [`Statement`] here as it finishes
//! and the table function reads the ones still kept.
//!
//! # What it costs
//!
//! The clocks cost what they cost before, because nothing new reads one but the monotonic clock
//! that numbers the record. What is new is one record written when a statement finishes, into a
//! ring of the thread's own under a lock only a reader of the table ever contends, so a statement
//! writes no cache line another thread writes, `engine-v4/13-the-point-path.md` section 13.5. The
//! record goes into a slot that already holds one from [`KEPT_STATEMENTS`] statements ago and the
//! text into the string that slot already has, so once the ring has gone round once a statement
//! allocates nothing here unless its text is longer than any statement the slot held before. The
//! text is cut at [`KEPT_TEXT`] bytes so that a statement carrying a megabyte of `VALUES` does not
//! copy a megabyte to be remembered. Reading the table takes every thread's ring in turn and keeps
//! the newest sixty four records.
//!
//! # Whose statements
//!
//! The process's, the same as `rudb_write_metrics()`. Two connections see each other's
//! statements, each one numbered by when it finished, which is what somebody tuning a server wants
//! and what a test has to know: a test that runs in parallel with others finds its own statements
//! by their text. The number is the nanoseconds since the process first kept a statement, with the
//! thread in the low bits so two threads finishing in the same nanosecond still differ. A thread
//! that ends leaves its newest records behind for the next read.
//!
//! A statement that failed is not kept. There is no document for it and a phase that was cut short
//! is not a phase. A statement answered from the native aggregate cache is not kept either, because
//! it never reaches the frontend this is measuring.

use std::cell::Cell;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock, PoisonError};
use std::time::Instant;

use crate::{Document, Split};

/// How many statements a process keeps for `rudb_statement_metrics()`.
pub const KEPT_STATEMENTS: usize = 64;

/// How much of a statement's text is kept, in bytes.
pub const KEPT_TEXT: usize = 4096;

/// One finished statement and what each phase of it cost, in wall nanoseconds.
///
/// The four frontend phases do not overlap and add up to the frontend. `optimize_ns` here is the
/// optimizer after the rewrites, which is the document's `optimize_ns` less its `rewrite_ns`, so
/// that a row is four disjoint numbers rather than one number and a part of it.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Statement {
    /// The number the process gave it, in the order statements finished, see the module.
    pub id: u64,
    /// The text as it was given, cut at [`KEPT_TEXT`] bytes.
    pub sql: String,
    /// Text to an abstract syntax tree.
    pub parse_ns: u64,
    /// Syntax tree to a bound logical plan.
    pub bind_ns: u64,
    /// The rewrites, which are the optimizer passes in front of join ordering.
    pub rewrite_ns: u64,
    /// The optimizer from join ordering on.
    pub optimize_ns: u64,
    /// The physical plan and the operator tree.
    pub physical_ns: u64,
    /// The part of `physical_ns` the compiled engine spent generating and compiling code.
    pub codegen_ns: u64,
    /// The part of `codegen_ns` that went on the compiled engine's physical plan.
    pub lower_ns: u64,
    /// The part of `codegen_ns` that went on generating QIR.
    pub qir_ns: u64,
    /// The part of `codegen_ns` the tier's backend took.
    pub backend_ns: u64,
    /// The QIR instructions the compiled engine generated, zero on the first engine.
    pub qir_insts: u64,
    /// The bytes of machine code it loaded.
    pub code_bytes: u64,
    /// Running it, with turning what it produced into a result.
    pub execute_ns: u64,
    /// The whole statement.
    pub total_ns: u64,
    /// CPU time across every thread, zero for a statement that did not run a plan.
    pub cpu_ns: u64,
    /// The execution by kind of work, all zero for a statement that did not run a plan. See
    /// [`Split`].
    pub split: Split,
}

impl Statement {
    /// The four frontend phases added up.
    #[must_use]
    pub fn frontend_ns(&self) -> u64 {
        self.parse_ns
            .saturating_add(self.bind_ns)
            .saturating_add(self.rewrite_ns)
            .saturating_add(self.optimize_ns)
    }

    /// Everything but the text and the number, copied out of a document.
    fn timed(&mut self, metrics: &Document) {
        let timing = &metrics.timing;
        self.parse_ns = timing.parse_ns;
        self.bind_ns = timing.bind_ns;
        self.rewrite_ns = timing.rewrite_ns;
        self.optimize_ns = timing.optimize_ns.saturating_sub(timing.rewrite_ns);
        self.physical_ns = timing.physical_ns;
        self.codegen_ns = timing.codegen_ns;
        self.lower_ns = timing.lower_ns;
        self.qir_ns = timing.qir_ns;
        self.backend_ns = timing.backend_ns;
        self.qir_insts = metrics.codegen.qir_insts;
        self.code_bytes = metrics.codegen.code_bytes;
        self.execute_ns = timing.execute_ns;
        self.total_ns = timing.total_ns;
        self.cpu_ns = metrics.resource.cpu_ns;
        self.split = metrics.split();
    }
}

/// One thread's ring. `written` is how many statements it has kept, `slots` fills to
/// [`KEPT_STATEMENTS`] and then goes round, and `last` is the time in the number it gave last.
#[derive(Debug, Default)]
struct Ring {
    written: u64,
    last: u64,
    slots: Vec<Statement>,
}

/// Every thread's ring, and what the threads that ended kept, newest last.
struct Rings {
    live: Vec<Arc<Mutex<Ring>>>,
    ended: Vec<Statement>,
}

static RINGS: Mutex<Rings> = Mutex::new(Rings { live: Vec::new(), ended: Vec::new() });

/// Bits of a statement's number that say which thread kept it.
const THREAD_BITS: u32 = 10;

/// The thread's ring, and the number that goes in the low bits of what it keeps.
struct Mine {
    ring: Arc<Mutex<Ring>>,
    thread: u64,
}

impl Mine {
    fn new() -> Self {
        static THREADS: AtomicU64 = AtomicU64::new(0);
        let ring = Arc::<Mutex<Ring>>::default();
        let mut rings = RINGS.lock().unwrap_or_else(PoisonError::into_inner);
        rings.retire();
        rings.live.push(Arc::clone(&ring));
        let thread = THREADS.fetch_add(1, Ordering::Relaxed) & ((1 << THREAD_BITS) - 1);
        Self { ring, thread }
    }
}

impl Rings {
    /// Moves the records of the threads that ended, whose ring nobody else holds, to `ended`, and
    /// keeps the newest of those.
    fn retire(&mut self) {
        let (gone, live): (Vec<_>, Vec<_>) = std::mem::take(&mut self.live)
            .into_iter()
            .partition(|ring| Arc::strong_count(ring) == 1);
        self.live = live;
        for ring in gone {
            let ring = ring.lock().unwrap_or_else(PoisonError::into_inner);
            self.ended.extend(ring.slots.iter().cloned());
        }
        newest(&mut self.ended);
    }
}

/// Sorts `kept` oldest first and drops all but the newest [`KEPT_STATEMENTS`].
fn newest(kept: &mut Vec<Statement>) {
    kept.sort_by_key(|statement| statement.id);
    let over = kept.len().saturating_sub(KEPT_STATEMENTS);
    kept.drain(..over);
}

thread_local! {
    /// How many statements this thread has kept, which is how a statement path that may or may not
    /// have reached a document finds out whether it did.
    static HERE: Cell<u64> = const { Cell::new(0) };

    static MINE: Mine = Mine::new();
}

/// Keep a statement that ran a plan, out of the document it produced.
pub fn remember(metrics: &Document) {
    keep(&metrics.query.sql, |slot| slot.timed(metrics));
}

/// Keep a statement that did not run a plan, such as `CREATE TABLE` or `SET`, with the two phases
/// it had and the whole of it.
pub fn remember_unplanned(sql: &str, parse_ns: u64, bind_ns: u64, total_ns: u64) {
    keep(sql, |slot| {
        *slot =
            Statement { id: slot.id, sql: std::mem::take(&mut slot.sql), ..Statement::default() };
        slot.parse_ns = parse_ns;
        slot.bind_ns = bind_ns;
        slot.total_ns = total_ns;
    });
}

/// How many statements this thread has kept so far.
#[must_use]
pub fn remembered_here() -> u64 {
    HERE.with(Cell::get)
}

/// The statements this process has kept, oldest first.
#[must_use]
pub fn recent_statements() -> Vec<Statement> {
    let mut rings = RINGS.lock().unwrap_or_else(PoisonError::into_inner);
    rings.retire();
    let mut kept = rings.ended.clone();
    for ring in &rings.live {
        kept.extend(ring.lock().unwrap_or_else(PoisonError::into_inner).slots.iter().cloned());
    }
    newest(&mut kept);
    kept
}

fn keep(sql: &str, fill: impl FnOnce(&mut Statement)) {
    HERE.with(|here| here.set(here.get().wrapping_add(1)));
    static START: OnceLock<Instant> = OnceLock::new();
    let now = u64::try_from(START.get_or_init(Instant::now).elapsed().as_nanos()).unwrap_or(0);
    let text = cut(sql);
    // A thread on its way out has dropped its ring, and its last statement is not kept.
    let _ = MINE.try_with(|mine| {
        let mut ring = mine.ring.lock().unwrap_or_else(PoisonError::into_inner);
        let time = now.max(ring.last + 1);
        ring.last = time;
        let at = usize::try_from(ring.written).unwrap_or(0) % KEPT_STATEMENTS;
        ring.written = ring.written.wrapping_add(1);
        if ring.slots.len() < KEPT_STATEMENTS {
            ring.slots.push(Statement::default());
        }
        let slot = &mut ring.slots[at];
        slot.id = time << THREAD_BITS | mine.thread;
        slot.sql.clear();
        slot.sql.push_str(text);
        fill(slot);
    });
}

/// The text cut at [`KEPT_TEXT`] bytes, back to the character it would have split.
fn cut(sql: &str) -> &str {
    if sql.len() <= KEPT_TEXT {
        return sql;
    }
    let mut end = KEPT_TEXT;
    while !sql.is_char_boundary(end) {
        end -= 1;
    }
    &sql[..end]
}

#[cfg(test)]
mod tests {
    use super::{KEPT_STATEMENTS, KEPT_TEXT, cut, recent_statements, remember, remembered_here};
    use crate::Document;

    #[test]
    fn a_statement_is_kept_with_its_phases_apart() {
        let mut metrics = Document::new("SELECT 'kept with its phases apart'");
        metrics.timing.parse_ns = 1;
        metrics.timing.bind_ns = 2;
        metrics.timing.optimize_ns = 10;
        metrics.timing.rewrite_ns = 3;
        metrics.timing.execute_ns = 20;
        metrics.timing.total_ns = 33;
        let before = remembered_here();
        remember(&metrics);
        assert_eq!(remembered_here(), before + 1);
        let kept = recent_statements();
        let row = kept.iter().rev().find(|row| row.sql == metrics.query.sql).expect("kept");
        assert_eq!((row.rewrite_ns, row.optimize_ns), (3, 7));
        assert_eq!(row.frontend_ns(), 13);
    }

    #[test]
    fn the_ring_keeps_the_newest_in_order() {
        for n in 0..KEPT_STATEMENTS + 5 {
            remember(&Document::new(&format!("SELECT {n} -- the ring")));
        }
        let kept = recent_statements();
        assert_eq!(kept.len(), KEPT_STATEMENTS);
        assert!(kept.windows(2).all(|pair| pair[0].id < pair[1].id), "oldest first");
    }

    #[test]
    fn a_long_text_is_cut_on_a_character() {
        let text = "\u{e9}".repeat(KEPT_TEXT);
        let kept = cut(&text);
        assert!(kept.len() <= KEPT_TEXT && kept.len() >= KEPT_TEXT - 1);
    }
}
