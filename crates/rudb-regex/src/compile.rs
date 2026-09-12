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

    /// Folds every byte a character in this set may begin with into a table.
    fn lead_bytes(&self, into: &mut [u64; 4]) {
        for code in 0..128u8 {
            if self.ascii >> code & 1 == 1 {
                into[code as usize / 64] |= 1 << (code % 64);
            }
        }
        // Above the ASCII line a character begins with a lead byte, and which lead byte depends on
        // how many bytes it takes. Working that out range by range is more care than a prefilter is
        // worth, so a set that reaches above the line takes every lead byte there is.
        if self.ranges.iter().any(|&(_, high)| high as u32 >= 128) {
            for byte in 0xc2..=0xf4u8 {
                into[byte as usize / 64] |= 1 << (byte % 64);
            }
        }
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

/// The bytes a match may begin with.
///
/// This is the prefilter, and it is the difference between asking one byte whether it is worth
/// running the machine and running the machine to find out. An unanchored search starts again at
/// every position, so a pattern that has to begin with `y` is otherwise the whole program stepped
/// once per character of a URL to learn what a compare would have said. ClickBench has several of
/// those and they are the queries this was written for.
///
/// It is a set and not a literal string, because a set is what the program hands over without any
/// analysis: walk from the entry through everything that reads no character, and collect the first
/// byte of everything that does. Anything the walk cannot pin down, which is a `.` or a pattern that
/// can match nothing at all, leaves the set as every byte and the skip as a no-op.
#[derive(Debug, Clone)]
pub(crate) struct First {
    bytes: [u64; 4],
    /// Whether every byte is in the set, in which case there is nothing to skip and the search runs
    /// from every position the way it did before this existed.
    any: bool,
    /// The one byte, when the set holds exactly one, because then the skip is a search for a byte
    /// rather than a table lookup per position.
    only: Option<u8>,
}

impl First {
    /// The next position at or after `at` where a match could begin, or `None` when the rest of the
    /// text holds no such position.
    pub(crate) fn skip(&self, bytes: &[u8], at: usize) -> Option<usize> {
        if self.any {
            return (at <= bytes.len()).then_some(at);
        }
        let rest = bytes.get(at..)?;
        let found = match self.only {
            Some(only) => rest.iter().position(|&byte| byte == only),
            None => rest.iter().position(|&byte| self.contains(byte)),
        };
        found.map(|found| at + found)
    }

    fn contains(&self, byte: u8) -> bool {
        self.bytes[byte as usize / 64] >> (byte % 64) & 1 == 1
    }

    fn anything() -> Self {
        Self { bytes: [u64::MAX; 4], any: true, only: None }
    }
}

/// Works out what a match may begin with by walking the program from its entry.
///
/// The walk follows everything that reads no character, because those decide nothing about the
/// first byte, and stops at everything that does. Reaching `Match` without reading anything means
/// the pattern matches the empty string, which it can do at every position, so there is nothing to
/// skip and the walk gives up. A `.` gives up for the same reason with a different cause.
fn first(insts: &[Inst], sets: &[Set]) -> First {
    let mut bytes = [0u64; 4];
    let mut seen = vec![false; insts.len()];
    let mut stack = vec![0usize];
    while let Some(pc) = stack.pop() {
        if seen[pc] {
            continue;
        }
        seen[pc] = true;
        match insts[pc] {
            Inst::Char(ch) => {
                let lead = ch.encode_utf8(&mut [0u8; 4]).as_bytes()[0];
                bytes[lead as usize / 64] |= 1 << (lead % 64);
            }
            Inst::Set(id) => sets[id].lead_bytes(&mut bytes),
            Inst::Any(_) | Inst::Match => return First::anything(),
            // An assertion decides nothing about the first byte and is followed through, which is
            // conservative in the only direction that is safe: a position the assertion would have
            // ruled out is still offered to the machine, which then rules it out itself.
            Inst::Assert(_) | Inst::Save(_) => stack.push(pc + 1),
            Inst::Split(one, other) => {
                stack.push(one);
                stack.push(other);
            }
            Inst::Jump(to) => stack.push(to),
        }
    }
    let count: u32 = bytes.iter().map(|word| word.count_ones()).sum();
    let only = (count == 1).then(|| {
        let word = bytes.iter().position(|&word| word != 0).unwrap_or(0);
        (word * 64 + bytes[word].trailing_zeros() as usize) as u8
    });
    First { bytes, any: false, only }
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
    /// What a match may begin with, which an unanchored search uses to skip positions.
    pub(crate) first: First,
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
    let first = first(&builder.insts, &builder.sets);
    Ok(Program { insts: builder.insts, sets: builder.sets, groups, anchored: anchored(ast), first })
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
    fn the_prefilter_knows_what_a_match_has_to_begin_with() {
        let one = program("yandex");
        assert_eq!(one.first.only, Some(b'y'));
        assert_eq!(one.first.skip(b"a yandex url", 0), Some(2));
        assert_eq!(one.first.skip(b"nothing here", 0), None);

        let either = program("(a|b)+c");
        assert_eq!(either.first.only, None, "two bytes is a table rather than a search");
        assert!(either.first.contains(b'a') && either.first.contains(b'b'));
        assert!(!either.first.contains(b'c'));
        assert_eq!(either.first.skip(b"zzzb", 0), Some(3));

        // An optional first piece means the byte after it can begin a match too, and a leading `.`
        // or a pattern that matches the empty string means anything can.
        assert!(program("(?:ab)?c").first.contains(b'c'));
        assert!(program(".c").first.any);
        assert!(program("a*").first.any);
        assert_eq!(program("a*").first.skip(b"zzz", 2), Some(2), "no skip is still a position");
        assert_eq!(program("a*").first.skip(b"zzz", 4), None, "and still stops past the end");
    }

    /// A skip has to land where a character begins, because the machine slices the text at wherever
    /// it lands. Continuation bytes are never lead bytes, so the set never holds one, and this is
    /// the test that says so rather than leaving it to be noticed by a panic in a query.
    #[test]
    fn the_prefilter_never_points_into_the_middle_of_a_character() {
        let program = program("é+");
        let text = "aaéb";
        let at = program.first.skip(text.as_bytes(), 0).expect("finds it");
        assert!(text.is_char_boundary(at));
        assert_eq!(at, 2);
        for byte in 0x80..0xc0u8 {
            assert!(!program.first.contains(byte), "{byte:#x} is a continuation byte");
        }
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
