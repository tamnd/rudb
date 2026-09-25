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
//! The clocks cost what they cost before, because nothing new reads one. What is new is one lock
//! taken and one record written when a statement finishes, and the record is written into a slot
//! that already holds a record from [`KEPT_STATEMENTS`] statements ago. The text goes into the
//! string that slot already has, so once the ring has gone round once a statement allocates nothing
//! here unless its text is longer than any statement the slot held before. The text is cut at
//! [`KEPT_TEXT`] bytes so that a statement carrying a megabyte of `VALUES` does not copy a megabyte
//! to be remembered. Nobody reading the table costs nothing further: the ring is written whether or
//! not anybody reads it, and reading it is a copy of sixty four records.
//!
//! # Whose statements
//!
//! The process's, the same as `rudb_write_metrics()`, because the one lock is what keeps the write
//! as cheap as it is. Two connections see each other's statements, each one numbered in the order
//! it finished, which is what somebody tuning a server wants and what a test has to know: a test
//! that runs in parallel with others finds its own statements by their text.
//!
//! A statement that failed is not kept. There is no document for it and a phase that was cut short
//! is not a phase. A statement answered from the native aggregate cache is not kept either, because
//! it never reaches the frontend this is measuring.

use std::cell::Cell;
use std::sync::{Mutex, PoisonError};

use crate::Document;

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
    /// The number the process gave it, in the order statements finished.
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
    /// Running it, with turning what it produced into a result.
    pub execute_ns: u64,
    /// The whole statement.
    pub total_ns: u64,
    /// CPU time across every thread, zero for a statement that did not run a plan.
    pub cpu_ns: u64,
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
        self.execute_ns = timing.execute_ns;
        self.total_ns = timing.total_ns;
        self.cpu_ns = metrics.resource.cpu_ns;
    }
}

/// The ring. `next` is the number the next statement gets and `slots` fills to
/// [`KEPT_STATEMENTS`] and then goes round.
struct Kept {
    next: u64,
    slots: Vec<Statement>,
}

static KEPT: Mutex<Kept> = Mutex::new(Kept { next: 0, slots: Vec::new() });

thread_local! {
    /// How many statements this thread has kept, which is how a statement path that may or may not
    /// have reached a document finds out whether it did.
    static HERE: Cell<u64> = const { Cell::new(0) };
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
    let kept = KEPT.lock().unwrap_or_else(PoisonError::into_inner);
    let at = usize::try_from(kept.next).unwrap_or(0) % KEPT_STATEMENTS;
    if kept.slots.len() < KEPT_STATEMENTS {
        return kept.slots.clone();
    }
    let (newer, older) = kept.slots.split_at(at);
    older.iter().chain(newer).cloned().collect()
}

fn keep(sql: &str, fill: impl FnOnce(&mut Statement)) {
    HERE.with(|here| here.set(here.get().wrapping_add(1)));
    let text = cut(sql);
    let mut kept = KEPT.lock().unwrap_or_else(PoisonError::into_inner);
    let id = kept.next;
    kept.next = kept.next.wrapping_add(1);
    let at = usize::try_from(id).unwrap_or(0) % KEPT_STATEMENTS;
    if kept.slots.len() < KEPT_STATEMENTS {
        kept.slots.push(Statement::default());
    }
    let slot = &mut kept.slots[at];
    slot.id = id;
    slot.sql.clear();
    slot.sql.push_str(text);
    fill(slot);
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
