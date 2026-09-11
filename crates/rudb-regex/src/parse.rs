//! The pattern to a syntax tree.
//!
//! A recursive descent parser over the characters of the pattern, written against what RE2 accepts
//! rather than against what Perl accepts, because RE2 is what DuckDB links and the difference is
//! visible: there are no backreferences in a pattern, no lookahead and no possessive quantifier, and
//! every one of those is a syntax error here rather than a feature that quietly does something else.
//!
//! The flags travel with the parser rather than with the compiler. `(?i)` changes what the
//! characters after it mean and it stops at the end of the group it was written in, so the fold has
//! to happen while the tree is being built, and a literal under a fold comes out of here as the two
//! character set it stands for rather than as a character with a flag attached.
//!
//! The error messages are RE2's, down to the fragment after the colon, because they reach a user
//! through DuckDB's `Invalid Input Error` and a test in the wild can assert on them.

use rudb_common::{Error, Result};

/// The most times a counted repetition may ask for, which is RE2's limit.
const MAX_REPEAT: u32 = 1000;

/// The syntax tree a pattern parses to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Ast {
    /// Matches everywhere and reads nothing. An empty branch of an alternation is this.
    Empty,
    /// One character.
    Literal(char),
    /// A set of characters.
    Class(Class),
    /// A dot, carrying whether it was written where the `s` flag was on.
    Any(bool),
    /// A position rather than a character.
    Assert(Assertion),
    /// A group, with the capture number it writes into, or `None` when it captures nothing.
    Group {
        /// The one based capture number, which is what `\1` in a replacement refers to.
        index: Option<usize>,
        /// What is inside the parentheses.
        inner: Box<Ast>,
    },
    /// One after another.
    Concat(Vec<Ast>),
    /// The earlier branch wins where both match, which is what makes this engine leftmost first
    /// rather than leftmost longest.
    Alternate(Vec<Ast>),
    /// A repetition. `most` of `None` is an open end.
    Repeat {
        /// What is repeated.
        inner: Box<Ast>,
        /// The fewest times it has to match.
        least: u32,
        /// The most times it may match, or `None` for no limit.
        most: Option<u32>,
        /// Whether it prefers to take the character or to leave it.
        greedy: bool,
    },
}

/// A place a match can be, as opposed to something a match reads.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Assertion {
    /// The start of the text, which is what `^` is unless `(?m)` is on.
    TextStart,
    /// The end of the text, which is what `$` is unless `(?m)` is on. RE2 is not Perl here: `$`
    /// does not match before a trailing newline.
    TextEnd,
    /// The start of the text or just after a newline.
    LineStart,
    /// The end of the text or just before a newline.
    LineEnd,
    /// Between a word character and something that is not one.
    WordBoundary,
    /// Anywhere a word boundary is not.
    NotWordBoundary,
}

/// A set of characters, written as ranges in the order they appeared.
///
/// Nothing is sorted, merged or complemented here. That happens once when the program is built,
/// which keeps the tree a faithful record of what was written and keeps the fast membership test in
/// one place.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct Class {
    /// Whether the set is everything except the ranges.
    pub(crate) negated: bool,
    /// The ranges, inclusive at both ends.
    pub(crate) ranges: Vec<(char, char)>,
}

impl Class {
    fn of(ranges: &[(char, char)]) -> Self {
        Self { negated: false, ranges: ranges.to_vec() }
    }
}

/// Which of the flags that can be written inside a pattern are on.
#[derive(Debug, Clone, Copy)]
struct Flags {
    case_insensitive: bool,
    dot_matches_newline: bool,
    multiline: bool,
    swap_greedy: bool,
}

/// Parses a pattern, returning the tree and how many capturing groups are in it.
///
/// # Errors
///
/// On anything RE2 refuses, with RE2's message.
pub(crate) fn parse(
    pattern: &str,
    case_insensitive: bool,
    dot_matches_newline: bool,
) -> Result<(Ast, usize)> {
    let mut parser = Parser {
        chars: pattern.chars().collect(),
        at: 0,
        groups: 0,
        flags: Flags {
            case_insensitive,
            dot_matches_newline,
            multiline: false,
            swap_greedy: false,
        },
    };
    let ast = parser.alternate()?;
    if parser.at < parser.chars.len() {
        // The only character that stops `alternate` without being consumed is a `)` that closes
        // nothing, since everything else is either an atom or an operator on one.
        return Err(fail("unexpected )", pattern.to_string()));
    }
    Ok((ast, parser.groups))
}

/// The tree for a pattern that is to be taken as the characters it is made of, which is DuckDB's
/// `l` option.
pub(crate) fn literal(pattern: &str, case_insensitive: bool) -> Ast {
    let pieces: Vec<Ast> = pattern.chars().map(|ch| character(ch, case_insensitive)).collect();
    match pieces.len() {
        0 => Ast::Empty,
        1 => pieces.into_iter().next().unwrap_or(Ast::Empty),
        _ => Ast::Concat(pieces),
    }
}

/// One character of a pattern, as itself or as the set it folds to.
fn character(ch: char, case_insensitive: bool) -> Ast {
    if !case_insensitive {
        return Ast::Literal(ch);
    }
    let mut ranges = vec![(ch, ch)];
    for other in [simple_lower(ch), simple_upper(ch)] {
        if other != ch {
            ranges.push((other, other));
        }
    }
    if ranges.len() == 1 { Ast::Literal(ch) } else { Ast::Class(Class::of(&ranges)) }
}

/// The one character a character folds down to, or itself when the fold is not one character.
///
/// `char::to_lowercase` is an iterator because a few letters lowercase to more than one, and a
/// multi character fold is not something a character set can hold. RE2 folds those through its
/// Unicode tables and this does not, which is a difference on a handful of letters outside the
/// alphabets any benchmark uses and is written down in the crate documentation rather than hidden.
fn simple_lower(ch: char) -> char {
    let mut folded = ch.to_lowercase();
    match (folded.next(), folded.next()) {
        (Some(one), None) => one,
        _ => ch,
    }
}

fn simple_upper(ch: char) -> char {
    let mut folded = ch.to_uppercase();
    match (folded.next(), folded.next()) {
        (Some(one), None) => one,
        _ => ch,
    }
}

/// An error with RE2's wording, which is a message and the piece of the pattern it is about.
fn fail(message: &str, fragment: String) -> Error {
    Error::invalid_input(format!("{message}: {fragment}"))
}

struct Parser {
    chars: Vec<char>,
    at: usize,
    groups: usize,
    flags: Flags,
}

impl Parser {
    fn peek(&self) -> Option<char> {
        self.chars.get(self.at).copied()
    }

    fn ahead(&self, by: usize) -> Option<char> {
        self.chars.get(self.at + by).copied()
    }

    fn bump(&mut self) -> Option<char> {
        let found = self.peek();
        if found.is_some() {
            self.at += 1;
        }
        found
    }

    fn eat(&mut self, ch: char) -> bool {
        if self.peek() == Some(ch) {
            self.at += 1;
            return true;
        }
        false
    }

    fn text(&self, from: usize, to: usize) -> String {
        self.chars[from..to.min(self.chars.len())].iter().collect()
    }

    fn rest(&self, from: usize) -> String {
        self.text(from, self.chars.len())
    }

    /// A sequence of branches separated by `|`.
    fn alternate(&mut self) -> Result<Ast> {
        let mut branches = vec![self.concat()?];
        while self.eat('|') {
            branches.push(self.concat()?);
        }
        if branches.len() == 1 {
            return Ok(branches.remove(0));
        }
        Ok(Ast::Alternate(branches))
    }

    /// One branch: atoms and the repetition operators on them, up to a `|`, a `)` or the end.
    fn concat(&mut self) -> Result<Ast> {
        let mut pieces: Vec<Ast> = Vec::new();
        while let Some(ch) = self.peek() {
            if ch == '|' || ch == ')' {
                break;
            }
            if self.repeat_ahead() {
                let Some(last) = pieces.pop() else {
                    let from = self.at;
                    self.skip_repeat();
                    return Err(fail(
                        "no argument for repetition operator",
                        self.text(from, self.at),
                    ));
                };
                pieces.push(self.repeat(last)?);
                continue;
            }
            pieces.push(self.atom()?);
        }
        Ok(match pieces.len() {
            0 => Ast::Empty,
            1 => pieces.remove(0),
            _ => Ast::Concat(pieces),
        })
    }

    /// Whether a repetition operator is at the cursor.
    fn repeat_ahead(&self) -> bool {
        matches!(self.peek(), Some('*' | '+' | '?')) || self.counted().is_some()
    }

    /// Steps over the repetition operator at the cursor, for the two messages that quote it.
    fn skip_repeat(&mut self) {
        if let Some((end, _, _)) = self.counted() {
            self.at = end;
            return;
        }
        self.at += 1;
        // A lazy marker belongs to the operator before it rather than being a second operator.
        self.eat('?');
    }

    /// `{n}`, `{n,}` or `{n,m}` at the cursor, as the position just past it and the bounds.
    ///
    /// `None` means the brace is an ordinary character, which is what RE2 does with `a{` and with
    /// `a{,3}`. This reads the pattern without moving the cursor, because whether the brace is an
    /// operator is exactly what the caller is asking.
    fn counted(&self) -> Option<(usize, u32, Option<u32>)> {
        if self.peek() != Some('{') {
            return None;
        }
        let (least, mut at) = self.number(self.at + 1)?;
        let most = if self.chars.get(at) == Some(&',') {
            at += 1;
            if self.chars.get(at) == Some(&'}') {
                None
            } else {
                let (value, next) = self.number(at)?;
                at = next;
                Some(value)
            }
        } else {
            Some(least)
        };
        if self.chars.get(at) != Some(&'}') {
            return None;
        }
        Some((at + 1, least, most))
    }

    /// A run of digits starting at `from`, as its value and the position after it.
    ///
    /// A run too long to be a repetition count comes back as one over the limit rather than as
    /// nothing, so that `a{99999999999}` is the size error it should be and not a literal brace.
    fn number(&self, from: usize) -> Option<(u32, usize)> {
        let mut at = from;
        let mut value: u32 = 0;
        while let Some(digit) = self.chars.get(at).and_then(|ch| ch.to_digit(10)) {
            value = value.saturating_mul(10).saturating_add(digit).min(MAX_REPEAT + 1);
            at += 1;
        }
        if at == from { None } else { Some((value, at)) }
    }

    /// The repetition operator at the cursor, applied to what came before it.
    fn repeat(&mut self, inner: Ast) -> Result<Ast> {
        let from = self.at;
        let (least, most) = match self.peek() {
            Some('*') => {
                self.at += 1;
                (0, None)
            }
            Some('+') => {
                self.at += 1;
                (1, None)
            }
            Some('?') => {
                self.at += 1;
                (0, Some(1))
            }
            _ => {
                let Some((end, least, most)) = self.counted() else {
                    return Err(Error::internal("a repetition operator that is not one"));
                };
                let text = self.text(self.at, end);
                self.at = end;
                if least > MAX_REPEAT || most.is_some_and(|most| most > MAX_REPEAT || most < least)
                {
                    return Err(fail("invalid repetition size", text));
                }
                (least, most)
            }
        };
        let mut greedy = !self.flags.swap_greedy;
        if self.eat('?') {
            greedy = !greedy;
        }
        // `a**` is an error rather than a star over a star, which is RE2 refusing the thing that in
        // Perl is a possessive quantifier and here would silently be something else.
        if self.repeat_ahead() {
            self.skip_repeat();
            return Err(fail("bad repetition operator", self.text(from, self.at)));
        }
        Ok(Ast::Repeat { inner: Box::new(inner), least, most, greedy })
    }

    /// One indivisible piece of a pattern.
    fn atom(&mut self) -> Result<Ast> {
        let from = self.at;
        let Some(ch) = self.bump() else {
            return Ok(Ast::Empty);
        };
        Ok(match ch {
            '(' => return self.group(from),
            '[' => Ast::Class(self.class(from)?),
            '.' => Ast::Any(self.flags.dot_matches_newline),
            '^' => Ast::Assert(if self.flags.multiline {
                Assertion::LineStart
            } else {
                Assertion::TextStart
            }),
            '$' => Ast::Assert(if self.flags.multiline {
                Assertion::LineEnd
            } else {
                Assertion::TextEnd
            }),
            '\\' => return self.escape(from),
            other => character(other, self.flags.case_insensitive),
        })
    }

    /// Everything that starts with `(`, with the cursor just past it.
    fn group(&mut self, from: usize) -> Result<Ast> {
        let saved = self.flags;
        let mut index = Some(self.groups + 1);
        if self.eat('?') {
            match self.peek() {
                Some(':') => {
                    self.at += 1;
                    index = None;
                }
                // `(?P<name>x)` and `(?<name>x)` capture the same way a plain group does. The name
                // is read and dropped, since nothing in the SQL surface looks a group up by name.
                Some('P') | Some('<') => {
                    self.eat('P');
                    if !self.eat('<') {
                        return Err(fail("invalid named capture group", self.rest(from)));
                    }
                    loop {
                        match self.bump() {
                            Some('>') => break,
                            Some(_) => {}
                            None => {
                                return Err(fail("invalid named capture group", self.rest(from)));
                            }
                        }
                    }
                }
                _ => {
                    if !self.inline_flags(from)? {
                        // `(?i)` on its own sets the flags for the rest of the group it is in, so
                        // this is the one path that deliberately does not put them back.
                        return Ok(Ast::Empty);
                    }
                    index = None;
                }
            }
        }
        if index.is_some() {
            self.groups += 1;
        }
        let inner = self.alternate()?;
        if !self.eat(')') {
            return Err(fail("missing )", self.rest(from)));
        }
        self.flags = saved;
        Ok(Ast::Group { index, inner: Box::new(inner) })
    }

    /// The flag letters of a `(?i)` or a `(?i:x)`, with the cursor just past the `?`.
    ///
    /// Returns whether a group follows, which is the difference between the two spellings.
    fn inline_flags(&mut self, from: usize) -> Result<bool> {
        let mut on = true;
        loop {
            match self.bump() {
                Some('i') => self.flags.case_insensitive = on,
                Some('s') => self.flags.dot_matches_newline = on,
                Some('m') => self.flags.multiline = on,
                Some('U') => self.flags.swap_greedy = on,
                Some('-') => on = false,
                Some(':') => return Ok(true),
                Some(')') => return Ok(false),
                _ => return Err(fail("invalid or unsupported Perl syntax", self.rest(from))),
            }
        }
    }

    /// A backslash and what follows it, with the cursor just past the backslash.
    fn escape(&mut self, from: usize) -> Result<Ast> {
        let Some(ch) = self.bump() else {
            return Err(fail("trailing \\ at end of regexp", self.rest(from)));
        };
        Ok(match ch {
            'd' | 'D' | 's' | 'S' | 'w' | 'W' => Ast::Class(perl_class(ch)),
            'b' => Ast::Assert(Assertion::WordBoundary),
            'B' => Ast::Assert(Assertion::NotWordBoundary),
            'A' => Ast::Assert(Assertion::TextStart),
            'z' => Ast::Assert(Assertion::TextEnd),
            other => character(self.escaped(other)?, self.flags.case_insensitive),
        })
    }

    /// The character an escape stands for, for the escapes that stand for one.
    ///
    /// Every punctuation mark escapes to itself, which is what makes `\.` a dot and `\\` a
    /// backslash. A letter or a digit that is not one of the escapes above is refused rather than
    /// taken as itself, so that `\1` is an error saying backreferences are not a thing here instead
    /// of a silent match against the digit one.
    fn escaped(&mut self, ch: char) -> Result<char> {
        Ok(match ch {
            'a' => '\u{7}',
            'f' => '\u{c}',
            'n' => '\n',
            'r' => '\r',
            't' => '\t',
            'v' => '\u{b}',
            'x' => self.hex()?,
            other if !other.is_alphanumeric() => other,
            other => return Err(fail("invalid escape sequence", format!("\\{other}"))),
        })
    }

    /// `\x41` or `\x{1f600}`, with the cursor just past the `x`.
    fn hex(&mut self) -> Result<char> {
        let from = self.at - 2;
        let mut value: u32 = 0;
        if self.eat('{') {
            let mut digits = 0;
            loop {
                match self.bump() {
                    Some('}') if digits > 0 => break,
                    Some(ch) => {
                        let Some(digit) = ch.to_digit(16) else {
                            return Err(fail("invalid escape sequence", self.text(from, self.at)));
                        };
                        value = value.saturating_mul(16).saturating_add(digit);
                        digits += 1;
                    }
                    None => return Err(fail("invalid escape sequence", self.rest(from))),
                }
            }
        } else {
            for _ in 0..2 {
                let Some(digit) = self.peek().and_then(|ch| ch.to_digit(16)) else {
                    return Err(fail("invalid escape sequence", self.text(from, self.at + 1)));
                };
                value = value * 16 + digit;
                self.at += 1;
            }
        }
        char::from_u32(value)
            .ok_or_else(|| fail("invalid escape sequence", self.text(from, self.at)))
    }

    /// The inside of a `[...]`, with the cursor just past the `[`.
    fn class(&mut self, from: usize) -> Result<Class> {
        let mut class = Class { negated: self.eat('^'), ranges: Vec::new() };
        let mut first = true;
        loop {
            let Some(ch) = self.peek() else {
                return Err(fail("missing ]", self.rest(from)));
            };
            // A `]` right after the bracket is the character rather than the end of the set, which
            // is how `[]a]` is the two character set RE2 makes of it.
            if ch == ']' && !first {
                self.at += 1;
                break;
            }
            first = false;
            if ch == '[' && self.ahead(1) == Some(':') {
                if let Some(ranges) = self.posix()? {
                    class.ranges.extend(ranges);
                    continue;
                }
            }
            match self.class_item(from)? {
                Item::Set(ranges) => class.ranges.extend(ranges),
                Item::Char(low) => {
                    let ranged =
                        self.peek() == Some('-') && self.ahead(1).is_some_and(|c| c != ']');
                    if !ranged {
                        class.ranges.push((low, low));
                        continue;
                    }
                    self.at += 1;
                    let start = self.at;
                    let Item::Char(high) = self.class_item(from)? else {
                        return Err(fail(
                            "invalid character class range",
                            self.text(start, self.at),
                        ));
                    };
                    if high < low {
                        return Err(fail("invalid character class range", format!("{low}-{high}")));
                    }
                    class.ranges.push((low, high));
                }
            }
        }
        if self.flags.case_insensitive {
            class.ranges = fold(&class.ranges);
        }
        Ok(class)
    }

    /// One character or one named set inside a `[...]`.
    fn class_item(&mut self, from: usize) -> Result<Item> {
        let Some(ch) = self.bump() else {
            return Err(fail("missing ]", self.rest(from)));
        };
        if ch != '\\' {
            return Ok(Item::Char(ch));
        }
        let Some(after) = self.bump() else {
            return Err(fail("missing ]", self.rest(from)));
        };
        Ok(match after {
            'd' | 'D' | 's' | 'S' | 'w' | 'W' => {
                let class = perl_class(after);
                // A negated set inside a set cannot stay negated, since the outer set is a union of
                // ranges, so it is turned into the ranges it stands for here.
                Item::Set(if class.negated { complement(&class.ranges) } else { class.ranges })
            }
            'b' => Item::Char('\u{8}'),
            other => Item::Char(self.escaped(other)?),
        })
    }

    /// A `[:alpha:]` at the cursor, or `None` when the bracket is an ordinary character.
    fn posix(&mut self) -> Result<Option<Vec<(char, char)>>> {
        let mut at = self.at + 2;
        let negated = self.chars.get(at) == Some(&'^');
        if negated {
            at += 1;
        }
        let start = at;
        while self.chars.get(at).is_some_and(|ch| ch.is_ascii_alphabetic()) {
            at += 1;
        }
        if self.chars.get(at) != Some(&':') || self.chars.get(at + 1) != Some(&']') {
            return Ok(None);
        }
        let name: String = self.chars[start..at].iter().collect();
        let Some(ranges) = posix_ranges(&name) else {
            // The fragment is the name and its brackets rather than the whole class, which is what
            // RE2 points at and so what DuckDB prints.
            return Err(fail("invalid character class range", self.text(self.at, at + 2)));
        };
        self.at = at + 2;
        Ok(Some(if negated { complement(ranges) } else { ranges.to_vec() }))
    }
}

/// What a `[...]` is made of, since `\d` inside one contributes a set and not a character.
enum Item {
    Char(char),
    Set(Vec<(char, char)>),
}

/// The Perl character classes, which RE2 keeps to ASCII even when the text is not.
fn perl_class(ch: char) -> Class {
    let ranges: &[(char, char)] = match ch.to_ascii_lowercase() {
        'd' => &[('0', '9')],
        // Tab, newline, form feed, carriage return and space, and deliberately not the vertical
        // tab, because RE2 leaves it out of `\s` and this is not the place to be more correct than
        // the thing we are compatible with.
        's' => &[('\t', '\n'), ('\u{c}', '\r'), (' ', ' ')],
        _ => &[('0', '9'), ('A', 'Z'), ('_', '_'), ('a', 'z')],
    };
    Class { negated: ch.is_ascii_uppercase(), ranges: ranges.to_vec() }
}

/// The POSIX named classes, as ASCII, which is again what RE2 does with them.
fn posix_ranges(name: &str) -> Option<&'static [(char, char)]> {
    Some(match name {
        "alnum" => &[('0', '9'), ('A', 'Z'), ('a', 'z')],
        "alpha" => &[('A', 'Z'), ('a', 'z')],
        "ascii" => &[('\0', '\u{7f}')],
        "blank" => &[('\t', '\t'), (' ', ' ')],
        "cntrl" => &[('\0', '\u{1f}'), ('\u{7f}', '\u{7f}')],
        "digit" => &[('0', '9')],
        "graph" => &[('!', '~')],
        "lower" => &[('a', 'z')],
        "print" => &[(' ', '~')],
        "punct" => &[('!', '/'), (':', '@'), ('[', '`'), ('{', '~')],
        "space" => &[('\t', '\r'), (' ', ' ')],
        "upper" => &[('A', 'Z')],
        "word" => &[('0', '9'), ('A', 'Z'), ('_', '_'), ('a', 'z')],
        "xdigit" => &[('0', '9'), ('A', 'F'), ('a', 'f')],
        _ => return None,
    })
}

/// Everything the ranges do not hold.
fn complement(ranges: &[(char, char)]) -> Vec<(char, char)> {
    let mut sorted = ranges.to_vec();
    sorted.sort_unstable();
    let mut out = Vec::new();
    let mut next = 0u32;
    for (low, high) in sorted {
        let low = low as u32;
        if low > next {
            if let (Some(from), Some(to)) = (char::from_u32(next), char::from_u32(low - 1)) {
                out.push((from, to));
            }
        }
        next = next.max(high as u32 + 1);
        // The gap between the two halves of the code point space holds nothing, so stepping over it
        // is what keeps `char::from_u32` from returning nothing on a perfectly good range.
        if next == 0xd800 {
            next = 0xe000;
        }
    }
    if let Some(from) = char::from_u32(next) {
        out.push((from, char::MAX));
    }
    out
}

/// The ranges plus the ranges they fold to, for a set written under `(?i)`.
fn fold(ranges: &[(char, char)]) -> Vec<(char, char)> {
    let mut out = ranges.to_vec();
    for &(low, high) in ranges {
        if low <= 'z' && high >= 'a' {
            out.push((low.max('a').to_ascii_uppercase(), high.min('z').to_ascii_uppercase()));
        }
        if low <= 'Z' && high >= 'A' {
            out.push((low.max('A').to_ascii_lowercase(), high.min('Z').to_ascii_lowercase()));
        }
        // A single character outside ASCII folds through the one character mapping, which is the
        // same one a literal uses. A range outside ASCII does not fold, because the letters in it
        // are not contiguous by case the way the two ASCII alphabets are.
        if low == high && !low.is_ascii() {
            for other in [simple_lower(low), simple_upper(low)] {
                if other != low {
                    out.push((other, other));
                }
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tree(pattern: &str) -> Ast {
        parse(pattern, false, false).expect("parses").0
    }

    fn message(pattern: &str) -> String {
        parse(pattern, false, false).expect_err("refused").to_string()
    }

    #[test]
    fn a_flat_pattern_is_a_concatenation_of_its_characters() {
        assert_eq!(
            tree("abc"),
            Ast::Concat(vec![Ast::Literal('a'), Ast::Literal('b'), Ast::Literal('c')])
        );
    }

    #[test]
    fn a_group_is_numbered_in_the_order_the_parenthesis_opens() {
        let (ast, groups) = parse("(a(b))(c)", false, false).expect("parses");
        assert_eq!(groups, 3);
        let Ast::Concat(pieces) = ast else { panic!("a concatenation") };
        let Ast::Group { index, .. } = pieces[0] else { panic!("a group") };
        assert_eq!(index, Some(1));
        let Ast::Group { index, .. } = pieces[1] else { panic!("a group") };
        assert_eq!(index, Some(3));
    }

    #[test]
    fn a_non_capturing_group_takes_no_number() {
        let (_, groups) = parse("(?:a)(b)", false, false).expect("parses");
        assert_eq!(groups, 1);
    }

    #[test]
    fn a_counted_repetition_reads_its_bounds() {
        assert_eq!(
            tree("a{2,4}"),
            Ast::Repeat {
                inner: Box::new(Ast::Literal('a')),
                least: 2,
                most: Some(4),
                greedy: true,
            }
        );
        let Ast::Repeat { least, most, .. } = tree("a{3,}") else { panic!("a repetition") };
        assert_eq!((least, most), (3, None));
        let Ast::Repeat { greedy, .. } = tree("a*?") else { panic!("a repetition") };
        assert!(!greedy, "a question mark after a star is laziness");
    }

    /// A brace that is not a count is the character, which is RE2's rule and is why the check has to
    /// look ahead rather than commit on the brace.
    #[test]
    fn a_brace_that_is_not_a_count_is_a_character() {
        assert_eq!(tree("a{"), Ast::Concat(vec![Ast::Literal('a'), Ast::Literal('{')]));
        assert_eq!(
            tree("a{,3}"),
            Ast::Concat(vec![
                Ast::Literal('a'),
                Ast::Literal('{'),
                Ast::Literal(','),
                Ast::Literal('3'),
                Ast::Literal('}'),
            ])
        );
    }

    /// Read off the DuckDB binary on server3, one message at a time, because these reach a user
    /// through `Invalid Input Error` and are the part of RE2 that is easiest to get almost right.
    #[test]
    fn the_messages_are_the_ones_duckdb_prints() {
        assert_eq!(message("("), "Invalid Input Error: missing ): (");
        assert_eq!(message("a[bc"), "Invalid Input Error: missing ]: [bc");
        assert_eq!(message("a**"), "Invalid Input Error: bad repetition operator: **");
        assert_eq!(message("*a"), "Invalid Input Error: no argument for repetition operator: *");
        assert_eq!(message("a\\q"), "Invalid Input Error: invalid escape sequence: \\q");
        assert_eq!(message("a{2,1}"), "Invalid Input Error: invalid repetition size: {2,1}");
        assert_eq!(message("a)b"), "Invalid Input Error: unexpected ): a)b");
    }

    #[test]
    fn a_bracket_right_after_the_opening_one_is_a_character() {
        let Ast::Class(class) = tree("[]a]") else { panic!("a class") };
        assert_eq!(class.ranges, vec![(']', ']'), ('a', 'a')]);
        assert!(!class.negated);
    }

    #[test]
    fn a_perl_class_inside_a_set_joins_it() {
        let Ast::Class(class) = tree("[\\dx]") else { panic!("a class") };
        assert_eq!(class.ranges, vec![('0', '9'), ('x', 'x')]);
    }

    #[test]
    fn a_negated_perl_class_inside_a_set_becomes_the_ranges_it_stands_for() {
        let Ast::Class(class) = tree("[\\D]") else { panic!("a class") };
        assert!(!class.negated, "the set is a union and cannot carry a negation");
        assert!(class.ranges.iter().any(|&(low, high)| low <= 'a' && 'a' <= high));
        assert!(!class.ranges.iter().any(|&(low, high)| low <= '5' && '5' <= high));
    }

    #[test]
    fn a_posix_class_is_read_and_a_bracket_that_is_not_one_is_a_character() {
        let Ast::Class(class) = tree("[[:digit:]]") else { panic!("a class") };
        assert_eq!(class.ranges, vec![('0', '9')]);
        assert_eq!(
            message("[[:nope:]]"),
            "Invalid Input Error: invalid character class range: [:nope:]"
        );
        let Ast::Class(class) = tree("[[a]") else { panic!("a class") };
        assert_eq!(class.ranges, vec![('[', '['), ('a', 'a')], "a bracket that is not a name");
    }

    #[test]
    fn an_inline_flag_lasts_to_the_end_of_its_group() {
        let (ast, _) = parse("a(?i)b", false, false).expect("parses");
        let Ast::Concat(pieces) = ast else { panic!("a concatenation") };
        assert_eq!(pieces[0], Ast::Literal('a'));
        assert!(matches!(pieces.last(), Some(Ast::Class(_))), "b folded and a did not");
    }

    #[test]
    fn a_fold_covers_both_cases_of_a_range() {
        let Ast::Class(class) = parse("[a-c]", true, false).expect("parses").0 else {
            panic!("a class")
        };
        assert_eq!(class.ranges, vec![('a', 'c'), ('A', 'C')]);
    }

    #[test]
    fn a_dot_remembers_the_flag_it_was_written_under() {
        assert_eq!(tree("."), Ast::Any(false));
        assert_eq!(parse(".", false, true).expect("parses").0, Ast::Any(true));
        assert_eq!(tree("(?s)."), Ast::Concat(vec![Ast::Empty, Ast::Any(true)]));
    }

    #[test]
    fn a_repetition_of_a_thousand_and_one_is_refused() {
        assert_eq!(message("a{1001}"), "Invalid Input Error: invalid repetition size: {1001}");
        assert!(parse("a{1000}", false, false).is_ok());
    }

    #[test]
    fn the_literal_tree_has_no_operators_in_it() {
        assert_eq!(
            literal("a.c", false),
            Ast::Concat(vec![Ast::Literal('a'), Ast::Literal('.'), Ast::Literal('c')])
        );
    }
}
