//! A backtracker with a memo, which is RE2's `BitState` and is the fast path here.
//!
//! The machine in `vm` runs every possibility at once, which is what makes it linear and is also
//! what makes it slow on the patterns people actually write. ClickBench query 29 asks for
//! `^https?://(?:www\.)?([^/]+)/.*$`, and that pattern is very nearly deterministic: at almost every
//! position in a URL there is exactly one way on. Pike's machine still pays for two thread lists, a
//! membership test per instruction it steps, and a reference counted slot vector that is cloned
//! every time a group records a position, which for a greedy `([^/]+)` is once per character of the
//! host. This machine walks the one way on and pays for none of that.
//!
//! What keeps it linear is the memo, which is one bit per instruction per position. A pair that has
//! been tried and failed fails again, because whether the rest of the program can match from a
//! position does not depend on where the groups have been on the way to it, so the bit is enough to
//! prune the retry. That bounds the work at the size of the program times the length of the text,
//! which is the bound `vm` has, and it is why `(a+)+b` against forty `a`s is answered at once here
//! rather than taking the age of the universe. It is also why this is not the machine for every
//! input: a bit per pair is memory, something has to cap it, and a large pattern over a long string
//! goes to `vm` instead, which needs no memo because it never visits a pair twice in the first
//! place.
//!
//! The capture slots are one array with an undo record pushed beside every branch that writes to it,
//! rather than one vector per thread. Backtracking past a `Save` pops the undo and puts the old
//! value back, which costs a write where the other machine costs an allocation.
//!
//! Priority is the same rule and comes out of the stack rather than out of an ordered list. The
//! second half of a split is pushed before the first, so the first is popped first and everything it
//! reaches is pushed on top of the second and walked before the second is reached. The first
//! `Match` a pop arrives at is therefore the leftmost first one, which is the answer RE2 gives.
//!
//! An unanchored search runs the whole thing again from each position in turn, and deliberately does
//! not clear the memo in between. A pair that failed from an earlier start fails from a later one
//! for the same reason it failed the first time, so keeping the bits is what holds the total at the
//! program times the text rather than the program times the text squared.

use std::cell::RefCell;

use crate::compile::{Inst, Program};
use crate::vm::holds;

/// The most bits the memo may take, which is RE2's quarter of a megabyte.
///
/// The memo is a bit per instruction per position, so this is one limit over two things: a small
/// pattern is allowed a long string and a large one is not. Past it `vm` runs instead.
const MAX_BITS: usize = 1 << 21;

/// Whether this pattern over this text is small enough to memo.
pub(crate) fn fits(program: &Program, text: &str) -> bool {
    program.insts.len().saturating_mul(text.len() + 1) <= MAX_BITS
}

/// Runs the program, with the same arguments and the same answer as [`crate::vm::search`].
pub(crate) fn search(
    program: &Program,
    text: &str,
    start: usize,
    whole: bool,
) -> Option<Vec<Option<usize>>> {
    CACHE.with(|cache| match cache.try_borrow_mut() {
        Ok(mut cache) => cache.search(program, text, start, whole),
        // Nothing on this path calls back into it, so the cache is never already borrowed. Building
        // one rather than panicking costs a few allocations on a call that does not happen.
        Err(_) => Cache::default().search(program, text, start, whole),
    })
}

thread_local! {
    /// The three buffers, kept between calls, because a column is a hundred million calls and the
    /// whole point of this machine is that it allocates nothing per row.
    static CACHE: RefCell<Cache> = RefCell::new(Cache::default());
}

/// One thing left to do.
#[derive(Debug, Clone, Copy)]
enum Job {
    /// Run this instruction at this position.
    Step { pc: usize, at: usize },
    /// Put this slot back to what it held, which is what backtracking past a `Save` means.
    Restore { slot: usize, value: Option<usize> },
}

/// Everything the machine needs that does not depend on the call.
#[derive(Debug, Default)]
struct Cache {
    /// A bit per instruction per position, indexed by the instruction times the number of positions
    /// plus the position.
    visited: Vec<u32>,
    /// The stack, which is what makes the recursion explicit and the depth a matter of memory
    /// rather than of the stack the thread was given.
    jobs: Vec<Job>,
    /// Where each group has been, as byte offsets, with one entry per slot rather than one per
    /// thread.
    slots: Vec<Option<usize>>,
}

impl Cache {
    fn search(
        &mut self,
        program: &Program,
        text: &str,
        start: usize,
        whole: bool,
    ) -> Option<Vec<Option<usize>>> {
        let stride = text.len() + 1;
        let words = program.insts.len() * stride / 32 + 1;
        self.visited.clear();
        self.visited.resize(words, 0);
        let width = 2 * (program.groups + 1);
        let bytes = text.as_bytes();
        // A pattern that has to start where the text does, and a whole text match, are tried once.
        // Everything else is tried at every position, which is what unanchored means.
        let everywhere = !program.anchored && !whole;
        let mut at = start;
        loop {
            if everywhere {
                // Every byte the prefilter skips is the whole machine not run. It only ever skips
                // past positions no match could begin at, so the answer is the same one a search
                // from every position gives, which is what the test against `vm` holds it to.
                at = program.first.skip(bytes, at)?;
            }
            self.slots.clear();
            self.slots.resize(width, None);
            if self.run(program, text, bytes, at, whole, stride) {
                return Some(self.slots.clone());
            }
            if !everywhere {
                return None;
            }
            let (_, size) = read(bytes, text, at)?;
            at += size;
        }
    }

    /// One attempt, from one starting position, leaving the answer in `slots`.
    fn run(
        &mut self,
        program: &Program,
        text: &str,
        bytes: &[u8],
        from: usize,
        whole: bool,
        stride: usize,
    ) -> bool {
        self.jobs.clear();
        self.jobs.push(Job::Step { pc: 0, at: from });
        while let Some(job) = self.jobs.pop() {
            let (pc, at) = match job {
                Job::Restore { slot, value } => {
                    self.slots[slot] = value;
                    continue;
                }
                Job::Step { pc, at } => (pc, at),
            };
            let cell = pc * stride + at;
            if self.visited[cell / 32] >> (cell % 32) & 1 == 1 {
                continue;
            }
            self.visited[cell / 32] |= 1 << (cell % 32);
            // The arms that read a character fall out of the match with what they read, and the
            // ones that read a position have already pushed whatever comes next and are done.
            let step = match program.insts[pc] {
                Inst::Char(want) => read(bytes, text, at).filter(|&(ch, _)| ch == want),
                Inst::Set(id) => {
                    read(bytes, text, at).filter(|&(ch, _)| program.sets[id].contains(ch))
                }
                Inst::Any(newline) => {
                    read(bytes, text, at).filter(|&(ch, _)| newline || ch != '\n')
                }
                Inst::Assert(assertion) => {
                    if holds(assertion, text, at) {
                        self.jobs.push(Job::Step { pc: pc + 1, at });
                    }
                    continue;
                }
                Inst::Save(slot) => {
                    // The undo goes on first so that it comes off last, after everything the rest
                    // of the program pushes on top of it has been walked and has failed.
                    if slot < self.slots.len() {
                        self.jobs.push(Job::Restore { slot, value: self.slots[slot] });
                        self.slots[slot] = Some(at);
                    }
                    self.jobs.push(Job::Step { pc: pc + 1, at });
                    continue;
                }
                Inst::Split(first, second) => {
                    self.jobs.push(Job::Step { pc: second, at });
                    self.jobs.push(Job::Step { pc: first, at });
                    continue;
                }
                Inst::Jump(to) => {
                    self.jobs.push(Job::Step { pc: to, at });
                    continue;
                }
                Inst::Match => {
                    // A whole text match that has text left over is not one, and this branch dies
                    // rather than the search stopping, because another one may still reach the end.
                    if whole && at != text.len() {
                        continue;
                    }
                    if let Some(end) = self.slots.get_mut(1) {
                        *end = Some(at);
                    }
                    return true;
                }
            };
            if let Some((_, size)) = step {
                self.jobs.push(Job::Step { pc: pc + 1, at: at + size });
            }
        }
        false
    }
}

/// The character at a position and how many bytes it took, or `None` at the end of the text.
///
/// ASCII is every byte of every string ClickBench holds and it is a load and a compare. Anything
/// else goes back through `str` and pays for the slice and the decode, which is the right way round
/// for how often each happens.
fn read(bytes: &[u8], text: &str, at: usize) -> Option<(char, usize)> {
    let &byte = bytes.get(at)?;
    if byte < 0x80 {
        return Some((byte as char, 1));
    }
    let ch = text[at..].chars().next()?;
    Some((ch, ch.len_utf8()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::compile::compile;
    use crate::parse::parse;
    use crate::vm;

    /// Every pattern in the test suites of the other two modules, plus the ones that are about the
    /// difference between the machines: a nested repetition, a group that takes no part, an empty
    /// match and a pattern over characters that are more than one byte.
    const PATTERNS: &[&str] = &[
        "a",
        "a|ab",
        "ab|a",
        "a+",
        "a+?",
        "a*",
        "a*?",
        "(a)(b)",
        "(a)|(b)",
        "(a)(b)?(c)",
        "x(y)?z",
        "^https?://(?:www\\.)?([^/]+)/.*$",
        "[^b]",
        "[a-cé]",
        "a{2,3}",
        "a{2}",
        "^a{3,}$",
        "(a+)+b",
        "(a*)*",
        "\\babc\\b",
        "\\Babc",
        "(?i)ABC",
        "(?s)a.b",
        "a.b",
        "(?m)^b",
        "a$",
        "^a",
        "",
        "é+",
        "<.*>",
        "<.*?>",
        "([a-z]+)([0-9]+)",
        "yandex",
        "(a|b)+",
        "(a|b)+c",
        "(?:ab)?c",
        "[xy]z",
        "\\d+",
        ".c",
    ];

    const TEXTS: &[&str] = &[
        "",
        "a",
        "ab",
        "ba",
        "aaa",
        "aaaa",
        "abc",
        "abcd",
        "xabcx",
        "xx abc123 yy",
        "http://www.example.com/a/b",
        "https://example.com/",
        "http://example.com",
        "aéés",
        "a\nb",
        "xyz",
        "xz",
        "<a><b>",
        "a yandex url",
        "zzzb",
        "aaéb",
        "abc123",
        "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
    ];

    /// The whole of the claim this module makes. It is not a faster engine unless it gives the same
    /// answer, and the answer is not just whether it matched: it is every capture slot, because
    /// `regexp_replace` reads them and a backtracker is exactly the kind of machine that gets
    /// priority subtly wrong.
    #[test]
    fn the_backtracker_and_the_machine_agree_on_every_pattern_over_every_text() {
        for pattern in PATTERNS {
            let (ast, groups) = parse(pattern, false, false).expect("parses");
            let program = compile(&ast, groups).expect("compiles");
            for text in TEXTS {
                for whole in [false, true] {
                    for start in 0..=text.len() {
                        if !text.is_char_boundary(start) {
                            continue;
                        }
                        assert_eq!(
                            search(&program, text, start, whole),
                            vm::search(&program, text, start, whole),
                            "{pattern:?} over {text:?} from {start} whole {whole}"
                        );
                    }
                }
            }
        }
    }

    /// The memo is the reason this is allowed to be a backtracker at all, so it is worth holding it
    /// to the pattern that is the whole argument against one. Without the memo this is two to the
    /// forty steps.
    #[test]
    fn the_pattern_that_kills_a_backtracking_engine_is_answered_at_once() {
        let (ast, groups) = parse("(a+)+b", false, false).expect("parses");
        let program = compile(&ast, groups).expect("compiles");
        let text = "a".repeat(2000);
        assert!(fits(&program, &text));
        assert_eq!(search(&program, &text, 0, false), None);
    }

    /// A pattern big enough to be worth an exponential number of steps is also big enough that the
    /// memo would not fit, and the point of the cap is that the answer is still linear on the other
    /// side of it.
    #[test]
    fn a_pattern_and_a_text_too_big_to_memo_are_handed_to_the_other_machine() {
        let (ast, groups) = parse("(a|b)+c", false, false).expect("parses");
        let program = compile(&ast, groups).expect("compiles");
        assert!(fits(&program, &"a".repeat(1000)));
        assert!(!fits(&program, &"a".repeat(MAX_BITS)));
    }
}
