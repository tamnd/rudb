//! The tree to a program the machine can run.
//!
//! The instruction set is Thompson's, in the form Pike's machine wants: something that reads a
//! character, something that splits into two possibilities in priority order, something that records
//! where a group started or ended, and something that says the match is done. There is no
//! instruction for a repetition, because a repetition is a split and a jump, and there is no
//! instruction for a group, because a group is two saves.
//!
//! A counted repetition is copied out rather than counted. `a{3,5}` compiles to five copies of `a`
//! with three of them mandatory, which is why there is a limit on how many a pattern may ask for and
//! a limit on how long the program may get. Both are checks with a message rather than an allocation
//! nobody sees coming.

use rudb_common::{Error, Result};

use crate::parse::{Assertion, Ast, Class};

/// The most instructions a pattern may compile to.
///
/// A thousand is the most one repetition may ask for and repetitions nest, so `(a{1000}){1000}` is a
/// million instructions without a limit here. RE2 has the same kind of budget and the same kind of
/// message for going over it.
const MAX_PROGRAM: usize = 100_000;

/// One instruction.
#[derive(Debug, Clone, Copy)]
pub(crate) enum Inst {
    /// Reads this character.
    Char(char),
    /// Reads a character in the set at this index.
    Set(usize),
    /// Reads any character, and the flag is whether a newline is one of them.
    Any(bool),
    /// Reads nothing and holds only where the assertion holds.
    Assert(Assertion),
    /// Records the current position in a capture slot.
    Save(usize),
    /// Two ways on, the first preferred over the second.
    Split(usize, usize),
    /// One way on.
    Jump(usize),
    /// The pattern has matched.
    Match,
}

/// A set of characters, in the shape the inner loop wants to ask about.
///
/// The ASCII half is a bitmap and the rest is a sorted list of ranges. Every pattern any benchmark
/// runs is ASCII, so the common answer is a shift and a mask, and the ranges are there so that a set
/// written over other alphabets is still right rather than still fast.
#[derive(Debug, Clone)]
pub(crate) struct Set {
    ascii: u128,
    ranges: Vec<(char, char)>,
}

impl Set {
    /// Normalizes a class from the tree: sorted, merged and with the negation applied, so that the
    /// question the machine asks is a lookup rather than a rule.
    fn new(class: &Class) -> Self {
        let mut ranges = class.ranges.clone();
        ranges.sort_unstable();
        let mut merged: Vec<(char, char)> = Vec::with_capacity(ranges.len());
        for (low, high) in ranges {
            match merged.last_mut() {
                // Touching counts as overlapping, so `[a-mn-z]` comes out as one range.
                Some(last) if low as u32 <= last.1 as u32 + 1 => {
                    if high > last.1 {
                        last.1 = high;
                    }
                }
                _ => merged.push((low, high)),
            }
        }
        if class.negated {
            merged = invert(&merged);
        }
        let mut ascii = 0u128;
        for &(low, high) in &merged {
            let mut code = low as u32;
            while code <= high as u32 && code < 128 {
                ascii |= 1u128 << code;
                code += 1;
            }
        }
        Self { ascii, ranges: merged }
    }

    /// Whether the set holds a character.
    pub(crate) fn contains(&self, ch: char) -> bool {
        let code = ch as u32;
        if code < 128 {
            return self.ascii >> code & 1 == 1;
        }
        self.ranges
            .binary_search_by(|&(low, high)| {
                if high < ch {
                    std::cmp::Ordering::Less
                } else if low > ch {
                    std::cmp::Ordering::Greater
                } else {
                    std::cmp::Ordering::Equal
                }
            })
            .is_ok()
    }
}

/// Everything a sorted, merged list of ranges does not hold.
fn invert(ranges: &[(char, char)]) -> Vec<(char, char)> {
    let mut out = Vec::with_capacity(ranges.len() + 1);
    let mut next = 0u32;
    for &(low, high) in ranges {
        let low = low as u32;
        if low > next {
            if let (Some(from), Some(to)) = (char::from_u32(next), char::from_u32(low - 1)) {
                out.push((from, to));
            }
        }
        next = next.max(high as u32 + 1);
        // The surrogate block is not made of characters, so a range that ends just under it has to
        // step over the hole rather than ask `char::from_u32` about a code point there.
        if next == 0xd800 {
            next = 0xe000;
        }
    }
    if let Some(from) = char::from_u32(next) {
        out.push((from, char::MAX));
    }
    out
}

/// A compiled pattern.
#[derive(Debug, Clone)]
pub(crate) struct Program {
    /// The instructions, entered at zero.
    pub(crate) insts: Vec<Inst>,
    /// The character sets the `Set` instructions index into.
    pub(crate) sets: Vec<Set>,
    /// How many capturing groups the pattern has, not counting the whole match.
    pub(crate) groups: usize,
    /// Whether every way through the program asserts the start of the text first.
    ///
    /// This is the difference between starting a thread at one position and starting one at every
    /// position, which on a long string is the difference between reading it once and reading it
    /// once per character. ClickBench query 29 is anchored, so this is on the measured path.
    pub(crate) anchored: bool,
}

/// Compiles a tree.
///
/// # Errors
///
/// If the program would be longer than the budget.
pub(crate) fn compile(ast: &Ast, groups: usize) -> Result<Program> {
    let mut builder = Builder { insts: Vec::new(), sets: Vec::new() };
    builder.push(Inst::Save(0))?;
    builder.emit(ast)?;
    builder.push(Inst::Save(1))?;
    builder.push(Inst::Match)?;
    Ok(Program { insts: builder.insts, sets: builder.sets, groups, anchored: anchored(ast) })
}

/// Whether the tree can only match at the start of the text.
fn anchored(ast: &Ast) -> bool {
    match ast {
        Ast::Assert(Assertion::TextStart) => true,
        Ast::Group { inner, .. } => anchored(inner),
        // The leading `(?i)` of a pattern is an empty node, and it is not what decides this.
        Ast::Concat(parts) => {
            parts.iter().find(|part| !matches!(part, Ast::Empty)).is_some_and(anchored)
        }
        Ast::Alternate(branches) => branches.iter().all(anchored),
        Ast::Repeat { inner, least, .. } => *least > 0 && anchored(inner),
        _ => false,
    }
}

struct Builder {
    insts: Vec<Inst>,
    sets: Vec<Set>,
}

impl Builder {
    fn push(&mut self, inst: Inst) -> Result<usize> {
        if self.insts.len() >= MAX_PROGRAM {
            return Err(Error::invalid_input("pattern too large - compile failed"));
        }
        self.insts.push(inst);
        Ok(self.insts.len() - 1)
    }

    fn here(&self) -> usize {
        self.insts.len()
    }

    fn emit(&mut self, ast: &Ast) -> Result<()> {
        match ast {
            Ast::Empty => {}
            Ast::Literal(ch) => {
                self.push(Inst::Char(*ch))?;
            }
            Ast::Class(class) => {
                self.sets.push(Set::new(class));
                let id = self.sets.len() - 1;
                self.push(Inst::Set(id))?;
            }
            Ast::Any(newline) => {
                self.push(Inst::Any(*newline))?;
            }
            Ast::Assert(assertion) => {
                self.push(Inst::Assert(*assertion))?;
            }
            Ast::Group { index, inner } => match index {
                Some(index) => {
                    self.push(Inst::Save(index * 2))?;
                    self.emit(inner)?;
                    self.push(Inst::Save(index * 2 + 1))?;
                }
                None => self.emit(inner)?,
            },
            Ast::Concat(parts) => {
                for part in parts {
                    self.emit(part)?;
                }
            }
            Ast::Alternate(branches) => self.alternate(branches)?,
            Ast::Repeat { inner, least, most, greedy } => {
                self.repeat(inner, *least, *most, *greedy)?;
            }
        }
        Ok(())
    }

    fn alternate(&mut self, branches: &[Ast]) -> Result<()> {
        let mut jumps = Vec::new();
        for (index, branch) in branches.iter().enumerate() {
            if index + 1 == branches.len() {
                self.emit(branch)?;
                break;
            }
            let split = self.push(Inst::Split(0, 0))?;
            let body = self.here();
            self.emit(branch)?;
            jumps.push(self.push(Inst::Jump(0))?);
            let next = self.here();
            self.insts[split] = Inst::Split(body, next);
        }
        let end = self.here();
        for jump in jumps {
            self.insts[jump] = Inst::Jump(end);
        }
        Ok(())
    }

    fn repeat(&mut self, inner: &Ast, least: u32, most: Option<u32>, greedy: bool) -> Result<()> {
        let Some(most) = most else {
            for _ in 0..least.saturating_sub(1) {
                self.emit(inner)?;
            }
            return if least == 0 { self.star(inner, greedy) } else { self.plus(inner, greedy) };
        };
        for _ in 0..least {
            self.emit(inner)?;
        }
        let mut splits = Vec::new();
        for _ in least..most {
            let split = self.push(Inst::Split(0, 0))?;
            let body = self.here();
            self.emit(inner)?;
            splits.push((split, body));
        }
        let end = self.here();
        for (split, body) in splits {
            self.insts[split] = branch(greedy, body, end);
        }
        Ok(())
    }

    fn star(&mut self, inner: &Ast, greedy: bool) -> Result<()> {
        let split = self.push(Inst::Split(0, 0))?;
        let body = self.here();
        self.emit(inner)?;
        self.push(Inst::Jump(split))?;
        let end = self.here();
        self.insts[split] = branch(greedy, body, end);
        Ok(())
    }

    fn plus(&mut self, inner: &Ast, greedy: bool) -> Result<()> {
        let body = self.here();
        self.emit(inner)?;
        let split = self.push(Inst::Split(0, 0))?;
        let end = self.here();
        self.insts[split] = branch(greedy, body, end);
        Ok(())
    }
}

/// A split that prefers to go round again, or to stop, depending on the greed.
fn branch(greedy: bool, body: usize, end: usize) -> Inst {
    if greedy { Inst::Split(body, end) } else { Inst::Split(end, body) }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::parse::parse;

    fn program(pattern: &str) -> Program {
        let (ast, groups) = parse(pattern, false, false).expect("parses");
        compile(&ast, groups).expect("compiles")
    }

    #[test]
    fn a_literal_compiles_to_one_instruction_per_character() {
        let program = program("abc");
        assert_eq!(program.insts.len(), 6, "two saves, three characters and a match");
        assert!(matches!(program.insts[1], Inst::Char('a')));
    }

    #[test]
    fn a_counted_repetition_is_copied_out() {
        assert_eq!(program("a{3}").insts.len(), 6, "three characters rather than a counter");
        assert!(program("a{2,5}").insts.len() > program("a{2,3}").insts.len());
    }

    #[test]
    fn a_greedy_star_prefers_the_body_and_a_lazy_one_prefers_the_exit() {
        let greedy = program("a*");
        let Inst::Split(first, second) = greedy.insts[1] else { panic!("a split") };
        assert!(first < second, "greedy goes into the body first");
        let lazy = program("a*?");
        let Inst::Split(first, second) = lazy.insts[1] else { panic!("a split") };
        assert!(first > second, "lazy leaves the body first");
    }

    #[test]
    fn a_pattern_that_starts_with_a_caret_is_anchored() {
        assert!(program("^a").anchored);
        assert!(program("^a|^b").anchored);
        assert!(!program("^a|b").anchored);
        assert!(!program("a^").anchored);
        assert!(program("(?:^a)+").anchored);
    }

    #[test]
    fn a_set_answers_from_a_bitmap_below_the_ascii_line_and_from_ranges_above_it() {
        let (ast, _) = parse("[a-cé]", false, false).expect("parses");
        let Ast::Class(class) = ast else { panic!("a class") };
        let set = Set::new(&class);
        assert!(set.contains('b'));
        assert!(!set.contains('d'));
        assert!(set.contains('é'));
        assert!(!set.contains('è'));
    }

    #[test]
    fn a_negated_set_is_inverted_once_rather_than_at_every_character() {
        let (ast, _) = parse("[^a-c]", false, false).expect("parses");
        let Ast::Class(class) = ast else { panic!("a class") };
        let set = Set::new(&class);
        assert!(!set.contains('b'));
        assert!(set.contains('d'));
        assert!(set.contains('\n'), "a negated set holds the newline, which RE2 does too");
        assert!(set.contains(char::MAX));
    }

    #[test]
    fn a_pattern_too_big_for_the_budget_is_refused_rather_than_built() {
        let (ast, groups) = parse("(a{1000}){1000}", false, false).expect("parses");
        let error = compile(&ast, groups).expect_err("too large");
        assert!(error.message().contains("pattern too large"), "{error}");
    }
}
