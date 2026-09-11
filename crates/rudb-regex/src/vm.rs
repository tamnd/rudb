//! Pike's virtual machine: the whole program run against every position at once.
//!
//! The machine keeps a list of threads, one per instruction it could be at, and steps all of them
//! over one character at a time. A list never holds the same instruction twice, so the number of
//! threads is bounded by the length of the program and the cost of a match is the length of the text
//! times the size of the program. That bound is the reason this is the machine to write first: a
//! backtracking engine is shorter and faster on the patterns people write by hand, and it is
//! quadratic or worse on the ones an attacker writes, which is not a property a database can have.
//!
//! Threads carry capture slots, which is what separates this from the plain Thompson simulation and
//! is what `regexp_replace` needs to answer `\1`. Slots are shared and copied on write, so a split
//! costs a pointer rather than a vector until one side of it records a position.
//!
//! Priority is the whole of the semantics. Threads are added in the order the splits prefer, and a
//! thread that reaches the end of the pattern cuts off every thread behind it in the list. That
//! makes the answer the leftmost first one, which is the answer Perl, RE2 and therefore DuckDB give,
//! and not the leftmost longest one POSIX asks for.

use std::rc::Rc;

use crate::compile::{Inst, Program};
use crate::parse::Assertion;

/// Where each group of a thread's match started and ended, as byte offsets into the text.
type Slots = Rc<Vec<Option<usize>>>;

/// Runs the program.
///
/// `start` is where the search begins and is not necessarily the start of the text, since a global
/// replacement searches again after each match and `^` still means the place the text begins.
/// `whole` asks for a match that covers the text exactly, which is `regexp_full_match`.
pub(crate) fn search(
    program: &Program,
    text: &str,
    start: usize,
    whole: bool,
) -> Option<Vec<Option<usize>>> {
    let width = 2 * (program.groups + 1);
    let mut current = List::new(program.insts.len());
    let mut next = List::new(program.insts.len());
    let mut matched: Option<Slots> = None;
    let mut at = start;
    loop {
        let ch = text[at..].chars().next();
        // A pattern that has to start at the start of the text, and a whole text match, get one
        // thread and not one per position. Everything else is searched for at every position, which
        // is what an unanchored match means.
        let again = matched.is_none() && !program.anchored && !whole;
        if matched.is_none() && (at == start || again) {
            let mut fresh = vec![None; width];
            fresh[0] = Some(at);
            add(program, text, &mut current, 0, at, Rc::new(fresh));
        }
        if current.threads.is_empty() && !again {
            break;
        }
        next.clear();
        let mut index = 0;
        while index < current.threads.len() {
            let (pc, slots) = current.threads[index].clone();
            index += 1;
            let reads = match program.insts[pc] {
                Inst::Char(want) => ch == Some(want),
                Inst::Set(id) => ch.is_some_and(|ch| program.sets[id].contains(ch)),
                Inst::Any(newline) => ch.is_some_and(|ch| newline || ch != '\n'),
                Inst::Match => {
                    // A whole text match that has text left over is not one, and the thread dies
                    // rather than the search stopping, because another branch may still reach the
                    // end.
                    if whole && ch.is_some() {
                        continue;
                    }
                    let mut done = slots;
                    if let Some(end) = Rc::make_mut(&mut done).get_mut(1) {
                        *end = Some(at);
                    }
                    matched = Some(done);
                    break;
                }
                // Everything else is a position rather than a character and was followed by `add`.
                _ => continue,
            };
            if let (true, Some(ch)) = (reads, ch) {
                add(program, text, &mut next, pc + 1, at + ch.len_utf8(), slots);
            }
        }
        let Some(ch) = ch else {
            break;
        };
        at += ch.len_utf8();
        std::mem::swap(&mut current, &mut next);
    }
    matched.map(|slots| slots.as_ref().clone())
}

/// Follows everything that reads no character, and adds what is left to the list.
///
/// The stack is explicit rather than a recursive call, because the depth is the size of the program
/// and a pattern is user input. Pushing the second half of a split before the first is what keeps
/// the list in priority order, since the stack hands the first one back first and its whole subtree
/// is walked before the second is reached.
fn add(program: &Program, text: &str, list: &mut List, pc: usize, at: usize, slots: Slots) {
    let mut stack = vec![(pc, slots)];
    while let Some((pc, slots)) = stack.pop() {
        if list.seen[pc] == list.generation {
            continue;
        }
        list.seen[pc] = list.generation;
        match program.insts[pc] {
            Inst::Jump(to) => stack.push((to, slots)),
            Inst::Split(first, second) => {
                stack.push((second, slots.clone()));
                stack.push((first, slots));
            }
            Inst::Save(slot) => {
                let mut updated = slots;
                if let Some(cell) = Rc::make_mut(&mut updated).get_mut(slot) {
                    *cell = Some(at);
                }
                stack.push((pc + 1, updated));
            }
            Inst::Assert(assertion) => {
                if holds(assertion, text, at) {
                    stack.push((pc + 1, slots));
                }
            }
            _ => list.threads.push((pc, slots)),
        }
    }
}

/// Whether an assertion holds at a position.
fn holds(assertion: Assertion, text: &str, at: usize) -> bool {
    match assertion {
        Assertion::TextStart => at == 0,
        Assertion::TextEnd => at == text.len(),
        Assertion::LineStart => at == 0 || before(text, at) == Some('\n'),
        Assertion::LineEnd => at == text.len() || after(text, at) == Some('\n'),
        Assertion::WordBoundary => word(before(text, at)) != word(after(text, at)),
        Assertion::NotWordBoundary => word(before(text, at)) == word(after(text, at)),
    }
}

fn before(text: &str, at: usize) -> Option<char> {
    text[..at].chars().next_back()
}

fn after(text: &str, at: usize) -> Option<char> {
    text[at..].chars().next()
}

/// Whether a character is one of the ones a word boundary is about, which RE2 keeps to ASCII.
fn word(ch: Option<char>) -> bool {
    ch.is_some_and(|ch| ch.is_ascii_alphanumeric() || ch == '_')
}

/// The threads at one position, with the set of instructions already in it.
///
/// The set is a generation counter per instruction rather than a bitmap that has to be cleared,
/// which is what makes clearing the list free. Over a hundred million rows that is a hundred million
/// clears that do not happen.
struct List {
    threads: Vec<(usize, Slots)>,
    seen: Vec<u32>,
    generation: u32,
}

impl List {
    fn new(size: usize) -> Self {
        Self { threads: Vec::with_capacity(size), seen: vec![0; size], generation: 1 }
    }

    fn clear(&mut self) {
        self.threads.clear();
        self.generation += 1;
        // Wrapping back to a generation still written in the table would say a thread is in a list
        // it is not in. Two to the thirty two steps is a long way off and the reset is cheap.
        if self.generation == u32::MAX {
            self.seen.iter_mut().for_each(|cell| *cell = 0);
            self.generation = 1;
        }
    }
}
