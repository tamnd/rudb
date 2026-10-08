//! The tree the parser builds, which is `parse`, `parsebranch` and `parseqatom` of `regcomp.c`.
//!
//! PostgreSQL builds an automaton and a tree of sub-expressions in the same pass, and the tree is
//! where its answers about groups come from. A node that has no group below it and no clash of
//! greed inside it is a leaf, which the dissect step never looks into. Everything else keeps its
//! shape: a concatenation of two parts, an alternation, an iteration or a capture. The rules that
//! decide which is which are copied here with their flags, because the flags are what decide where
//! the groups land. Each node carries the pattern of everything below it as an [`Ast`] with no
//! groups in it, which `run` compiles to the program that node is matched with.

use super::ctype::{PgClass, PgCtype};
use super::lex::{Cls, Lexer, Tok};
use super::{Cflags, Code};
use crate::parse::{Assertion, Ast, Class, complement};

/// The node prefers the longer match.
const LONGER: u8 = 1;
/// The node prefers the shorter match.
pub(super) const SHORTER: u8 = 2;
/// Both preferences are somewhere below the node.
const MIXED: u8 = 4;
/// A capturing group is at the node or below it.
const CAP: u8 = 8;
/// A back reference is at the node or below it.
const BACKR: u8 = 16;

/// The most times a bound may ask for, which is `DUPMAX`.
const DUPMAX: u32 = 255;

/// How deep the parser may recurse before it calls the pattern too complex.
const MAX_DEPTH: usize = 1000;

/// `UP`: the flags a parent takes from a child.
fn up(flags: u8) -> u8 {
    let mixed = if flags & LONGER != 0 && flags & SHORTER != 0 { MIXED } else { 0 };
    (flags & (MIXED | CAP | BACKR)) | mixed
}

/// `MESSY`: whether a node has to keep its shape for the dissect step.
fn messy(flags: u8) -> bool {
    flags & (MIXED | CAP | BACKR) != 0
}

/// `PREF`: the preference of a node itself.
fn pref(flags: u8) -> u8 {
    flags & (LONGER | SHORTER)
}

/// `COMBINE`: the flags of two nodes together, with the preference of the first where it has one.
fn combine(one: u8, two: u8) -> u8 {
    up(one | two) | if pref(one) != 0 { pref(one) } else { pref(two) }
}

/// What a node does with its children.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Op {
    /// `=`: a part the dissect step does not look into.
    Leaf,
    /// `.`: two parts, one after the other.
    Concat,
    /// `|`: the first of the parts that matches.
    Alt,
    /// `*`: the one part, repeated.
    Iter { min: u32, max: Option<u32> },
    /// `(`: a capture around a part that already captures, as in `((x))`.
    Capture,
}

#[derive(Debug, Clone)]
pub(super) struct Node {
    pub(super) op: Op,
    pub(super) flags: u8,
    /// The group the node captures, or zero.
    pub(super) capno: usize,
    pub(super) children: Vec<Node>,
    /// The pattern of the node and everything below it, with no groups.
    pub(super) ast: Ast,
}

impl Node {
    fn leaf(flags: u8, ast: Ast) -> Self {
        Self { op: Op::Leaf, flags, capno: 0, children: Vec::new(), ast }
    }

    fn concat(flags: u8, left: Node, right: Node) -> Self {
        let ast = join(left.ast.clone(), right.ast.clone());
        Self { op: Op::Concat, flags, capno: 0, children: vec![left, right], ast }
    }
}

/// Two patterns one after the other, flattened so a long branch stays one sequence.
fn join(left: Ast, right: Ast) -> Ast {
    let mut parts = Vec::new();
    for ast in [left, right] {
        match ast {
            Ast::Empty => {}
            Ast::Concat(inner) => parts.extend(inner),
            other => parts.push(other),
        }
    }
    match parts.len() {
        0 => Ast::Empty,
        1 => parts.pop().unwrap_or(Ast::Empty),
        _ => Ast::Concat(parts),
    }
}

/// `x{m,n}` as a pattern, where the greed does not matter because a node is matched for the
/// longest or the shortest end as its flags say, not by the order of its branches.
fn repeat(ast: Ast, least: u32, most: Option<u32>) -> Ast {
    match (least, most) {
        (1, Some(1)) => ast,
        (0, Some(0)) => Ast::Empty,
        _ => Ast::Repeat { inner: Box::new(ast), least, most, greedy: true },
    }
}

/// Parses a pattern, returning its tree and how many groups it captures.
///
/// A pattern with a back reference parses as PostgreSQL parses it, so that a syntax error after
/// the back reference is still the error raised, and is refused only at the end.
pub(super) fn parse(
    pattern: &str,
    cflags: Cflags,
    ctype: &dyn PgCtype,
) -> Result<(Node, usize), Code> {
    let lexer = Lexer::start(pattern, cflags)?;
    let mut parser = Parser { lexer, ctype, closed: Vec::new(), backref: false, depth: 0 };
    let node = parser.alternation(false, false)?;
    if parser.backref {
        return Err(Code::Backref);
    }
    Ok((node, parser.lexer.groups as usize))
}

struct Parser<'c> {
    lexer: Lexer,
    /// The classes and the case mapping of the collation of the call.
    ctype: &'c dyn PgCtype,
    /// Which groups have closed, which is what a back reference may refer to.
    closed: Vec<bool>,
    backref: bool,
    depth: usize,
}

impl<'c> Parser<'c> {
    fn advance(&mut self) -> Result<(), Code> {
        self.lexer.advance()
    }

    fn cflags(&self) -> Cflags {
        self.lexer.cflags
    }

    /// `parse`: branches separated by `|`, up to the end or up to a `)` when `paren` is set.
    fn alternation(&mut self, paren: bool, lacon: bool) -> Result<Node, Code> {
        self.depth += 1;
        if self.depth > MAX_DEPTH {
            return Err(Code::TooBig);
        }
        let mut flags = LONGER;
        let mut branches = Vec::new();
        loop {
            let branch = self.branch(paren, lacon)?;
            flags |= up(flags | branch.flags);
            branches.push(branch);
            if self.lexer.next != Tok::Bar {
                break;
            }
            self.advance()?;
        }
        if paren && self.lexer.next != Tok::RParen {
            return Err(Code::Paren);
        }
        self.depth -= 1;
        if branches.len() == 1 {
            return Ok(branches.pop().unwrap_or_else(|| Node::leaf(0, Ast::Empty)));
        }
        let ast = Ast::Alternate(branches.iter().map(|branch| branch.ast.clone()).collect());
        if !messy(flags) {
            return Ok(Node::leaf(flags, ast));
        }
        Ok(Node { op: Op::Alt, flags, capno: 0, children: branches, ast })
    }

    fn stops(&self, paren: bool) -> bool {
        matches!(self.lexer.next, Tok::Bar | Tok::Eos) || (paren && self.lexer.next == Tok::RParen)
    }

    /// `parsebranch`: quantified atoms one after the other.
    fn branch(&mut self, paren: bool, lacon: bool) -> Result<Node, Code> {
        let mut top = Node::leaf(0, Ast::Empty);
        let mut seen = false;
        while !self.stops(paren) {
            top = self.qatom(paren, lacon, top, !seen)?;
            seen = true;
        }
        Ok(top)
    }

    /// `parseqatom`: one constraint, or one atom with its quantifier, added to `top`. Where the
    /// atom has to keep its shape the rest of the branch is parsed here too, as PostgreSQL does.
    fn qatom(
        &mut self,
        paren: bool,
        lacon: bool,
        mut top: Node,
        first: bool,
    ) -> Result<Node, Code> {
        let cflags = self.cflags();
        let constraint = match self.lexer.next {
            Tok::Caret => Some(Ast::Assert(if cflags.nlanch {
                Assertion::LineStart
            } else {
                Assertion::TextStart
            })),
            Tok::Dollar => Some(Ast::Assert(if cflags.nlanch {
                Assertion::LineEnd
            } else {
                Assertion::TextEnd
            })),
            Tok::TextStart => Some(Ast::Assert(Assertion::TextStart)),
            Tok::TextEnd => Some(Ast::Assert(Assertion::TextEnd)),
            Tok::WordStart => Some(Ast::Assert(Assertion::WordStart)),
            Tok::WordEnd => Some(Ast::Assert(Assertion::WordEnd)),
            Tok::Boundary => Some(Ast::Assert(Assertion::AnyWordBoundary)),
            Tok::NotBoundary => Some(Ast::Assert(Assertion::NotAnyWordBoundary)),
            Tok::Look { ahead, negated } => {
                self.advance()?;
                let inner = self.alternation(true, true)?;
                Some(Ast::Look { ahead, negated, inner: Box::new(inner.ast) })
            }
            Tok::Star(_) | Tok::Plus(_) | Tok::Quest(_) | Tok::LBrace => {
                return Err(Code::BadRpt);
            }
            _ => None,
        };
        if let Some(constraint) = constraint {
            self.advance()?;
            top.ast = join(top.ast, constraint);
            return Ok(top);
        }

        // The atom, where `node` is set for one that came out of parentheses.
        let mut capturing = false;
        let mut backref = false;
        let mut node: Option<Node> = None;
        let atom = match self.lexer.next {
            Tok::RParen => {
                // A `)` that closes nothing is a character in an ERE, by a botch of the standard.
                if !(cflags.extended && !cflags.advf) {
                    return Err(Code::Paren);
                }
                self.advance()?;
                self.character(')', cflags.icase)
            }
            Tok::Plain(ch) => {
                self.advance()?;
                self.character(ch, cflags.icase)
            }
            Tok::LBracket(positive) => {
                let class = self.bracket(positive)?;
                self.advance()?;
                Ast::Class(class)
            }
            Tok::ClassS(cls) => {
                self.advance()?;
                Ast::Class(Class::of(self.parts(cls, cflags.icase)))
            }
            Tok::ClassC(cls) => {
                self.advance()?;
                Ast::Class(Class { negated: true, ..Class::of(self.parts(cls, cflags.icase)) })
            }
            Tok::Dot => {
                self.advance()?;
                Ast::Any(!cflags.nlstop)
            }
            Tok::LParen(cap) => {
                let cap = cap && !lacon;
                let mut subno = 0;
                if cap {
                    self.lexer.groups += 1;
                    subno = self.lexer.groups as usize;
                }
                self.advance()?;
                let mut inner = self.alternation(true, lacon)?;
                self.advance()?;
                if cap {
                    if inner.capno == 0 {
                        inner.flags |= CAP;
                        inner.capno = subno;
                    } else {
                        let ast = inner.ast.clone();
                        inner = Node {
                            op: Op::Capture,
                            flags: inner.flags | CAP,
                            capno: subno,
                            children: vec![inner],
                            ast,
                        };
                    }
                    if self.closed.len() <= subno {
                        self.closed.resize(subno + 1, false);
                    }
                    self.closed[subno] = true;
                }
                capturing = cap;
                let ast = inner.ast.clone();
                node = Some(inner);
                ast
            }
            Tok::Backref(number) => {
                let number = number as usize;
                if lacon || !self.closed.get(number).copied().unwrap_or(false) {
                    return Err(Code::Subreg);
                }
                self.backref = true;
                backref = true;
                self.advance()?;
                Ast::Empty
            }
            _ => return Err(Code::Assert),
        };

        // The quantifier.
        let quantified =
            matches!(self.lexer.next, Tok::Star(_) | Tok::Plus(_) | Tok::Quest(_) | Tok::LBrace);
        let (least, most, qprefer) = match self.lexer.next {
            Tok::Star(greedy) => (0, None, prefer(greedy)),
            Tok::Plus(greedy) => (1, None, prefer(greedy)),
            Tok::Quest(greedy) => (0, Some(1), prefer(greedy)),
            Tok::LBrace => {
                self.advance()?;
                let least = self.number()?;
                let (most, qprefer) = if self.lexer.next == Tok::Comma {
                    self.advance()?;
                    let most = if matches!(self.lexer.next, Tok::Digit(_)) {
                        Some(self.number()?)
                    } else {
                        None
                    };
                    if most.is_some_and(|most| least > most) {
                        return Err(Code::BadBr);
                    }
                    let Tok::RBrace(greedy) = self.lexer.next else { return Err(Code::BadBr) };
                    (most, prefer(greedy))
                } else {
                    (Some(least), 0)
                };
                if !matches!(self.lexer.next, Tok::RBrace(_)) {
                    return Err(Code::BadBr);
                }
                (least, most, qprefer)
            }
            _ => (1, Some(1), 0),
        };
        if quantified {
            self.advance()?;
        }

        // `{0}` cancels the atom, groups and all.
        if least == 0 && most == Some(0) {
            return Ok(top);
        }

        let atom_flags = node.as_ref().map_or(0, |node| node.flags);
        let f = top.flags | qprefer | atom_flags;
        if !capturing && !backref && !messy(up(f)) {
            top.ast = join(top.ast, repeat(atom, least, most));
            top.flags = f;
            return Ok(top);
        }

        // The messy part: a capture, or a clash of greed, or an atom with one of those inside.
        let atom = node.unwrap_or_else(|| Node::leaf(if backref { BACKR } else { 0 }, atom));
        let mut tflags = combine(qprefer, atom.flags);
        let prefix = Node::leaf(top.flags, top.ast);
        let shape = atom.flags & (LONGER | SHORTER | MIXED);
        let child = if least == 1
            && most == Some(1)
            && (qprefer == 0 || shape == 0 || qprefer == shape)
        {
            atom
        } else if atom.flags & (CAP | BACKR) == 0 {
            let flags = combine(qprefer, atom.flags);
            Node::leaf(flags, repeat(atom.ast, least, most))
        } else if least > 0 {
            let flags = combine(qprefer, atom.flags);
            let before =
                Node::leaf(pref(flags), repeat(atom.ast.clone(), least - 1, most.map(|n| n - 1)));
            Node::concat(flags, before, atom)
        } else {
            let flags = combine(qprefer, atom.flags);
            let ast = repeat(atom.ast.clone(), least, most);
            Node {
                op: Op::Iter { min: least, max: most },
                flags,
                capno: 0,
                children: vec![atom],
                ast,
            }
        };

        if !self.stops(paren) {
            let rest = self.branch(paren, lacon)?;
            tflags |= combine(tflags, rest.flags);
            let topflags = top.flags | combine(top.flags, tflags);
            if first {
                return Ok(Node::concat(topflags, child, rest));
            }
            if child.op == Op::Leaf && rest.op == Op::Leaf && !messy(up(child.flags | rest.flags)) {
                let merged =
                    Node::leaf(combine(child.flags, rest.flags), join(child.ast, rest.ast));
                return Ok(Node::concat(topflags, prefix, merged));
            }
            return Ok(Node::concat(topflags, prefix, Node::concat(tflags, child, rest)));
        }
        let topflags = top.flags | combine(top.flags, child.flags);
        if first {
            return Ok(child);
        }
        Ok(Node::concat(topflags, prefix, child))
    }

    /// `scannum`: the number of a bound.
    fn number(&mut self) -> Result<u32, Code> {
        let mut n = 0;
        while let Tok::Digit(digit) = self.lexer.next {
            if n >= DUPMAX {
                break;
            }
            n = n * 10 + digit;
            self.advance()?;
        }
        if matches!(self.lexer.next, Tok::Digit(_)) || n > DUPMAX {
            return Err(Code::BadBr);
        }
        Ok(n)
    }

    /// `bracket` and `cbracket`: the items up to the `]`, which is left as the next token.
    fn bracket(&mut self, positive: bool) -> Result<Class, Code> {
        let icase = self.cflags().icase;
        let mut ranges: Vec<(char, char)> = Vec::new();
        let mut complemented: Vec<Cls> = Vec::new();
        self.advance()?;
        while !matches!(self.lexer.next, Tok::RBracket | Tok::Eos) {
            self.item(icase, &mut ranges, &mut complemented)?;
        }
        for cls in complemented {
            ranges.extend(complement(self.parts(cls, icase)));
        }
        if !positive && self.cflags().nlstop {
            ranges.push(('\n', '\n'));
        }
        Ok(Class { negated: !positive, ranges })
    }

    /// `brackpart`: one item of a bracket, or one range.
    fn item(
        &mut self,
        icase: bool,
        ranges: &mut Vec<(char, char)>,
        complemented: &mut Vec<Cls>,
    ) -> Result<(), Code> {
        let start = match self.lexer.next {
            Tok::Range(_) => return Err(Code::Range),
            Tok::Plain(ch) => {
                self.advance()?;
                if !matches!(self.lexer.next, Tok::Range(_)) {
                    self.add_cases(ranges, ch, icase);
                    return Ok(());
                }
                ch
            }
            Tok::Collel => {
                let name = self.lexer.name()?;
                element(&name)?
            }
            Tok::Eclass => {
                let name = self.lexer.name()?;
                self.add_cases(ranges, element(&name)?, icase);
                return Ok(());
            }
            Tok::Cclass => {
                let name = self.lexer.name()?;
                if name.is_empty() {
                    return Err(Code::Ctype);
                }
                let cls = Cls::named(&name).ok_or(Code::Ctype)?;
                ranges.extend_from_slice(self.parts(cls, icase));
                return Ok(());
            }
            Tok::ClassS(cls) => {
                self.advance()?;
                ranges.extend_from_slice(self.parts(cls, icase));
                return Ok(());
            }
            Tok::ClassC(cls) => {
                self.advance()?;
                complemented.push(cls);
                return Ok(());
            }
            _ => return Err(Code::Assert),
        };
        let end = if matches!(self.lexer.next, Tok::Range(_)) {
            self.advance()?;
            match self.lexer.next {
                Tok::Plain(ch) | Tok::Range(ch) => {
                    self.advance()?;
                    ch
                }
                Tok::Collel => {
                    let name = self.lexer.name()?;
                    element(&name)?
                }
                _ => return Err(Code::Range),
            }
        } else {
            start
        };
        self.range(ranges, start, end, icase)
    }

    /// One character, or under case folding the set `allcases` gives, which is its lower and
    /// upper case by the collation and not necessarily the character itself: a title case letter
    /// matches neither its own spelling nor anything but the other two.
    fn character(&self, ch: char, icase: bool) -> Ast {
        if !icase {
            return Ast::Literal(ch);
        }
        let (lower, upper) = (self.ctype.lower(ch), self.ctype.upper(ch));
        if lower == upper {
            return Ast::Literal(lower);
        }
        Ast::Class(Class::of(&[(lower, lower), (upper, upper)]))
    }

    fn add_cases(&self, ranges: &mut Vec<(char, char)>, ch: char, icase: bool) {
        if !icase {
            ranges.push((ch, ch));
            return;
        }
        let (lower, upper) = (self.ctype.lower(ch), self.ctype.upper(ch));
        ranges.push((lower, lower));
        ranges.push((upper, upper));
    }

    /// `range`: the characters from one to the other, and under case folding the other case of
    /// each one outside the range, with PostgreSQL's limit on how many that may be.
    fn range(
        &self,
        ranges: &mut Vec<(char, char)>,
        low: char,
        high: char,
        icase: bool,
    ) -> Result<(), Code> {
        if low > high {
            return Err(Code::Range);
        }
        ranges.push((low, high));
        if !icase {
            return Ok(());
        }
        let span = high as u32 - low as u32 + 1;
        let space = if span > 100_000 { 100_000 } else { span } as usize;
        let mut added = 0usize;
        for ch in (low as u32..=high as u32).filter_map(char::from_u32) {
            for other in [self.ctype.lower(ch), self.ctype.upper(ch)] {
                if other != ch && (other < low || other > high) {
                    if added >= space {
                        return Err(Code::TooBig);
                    }
                    ranges.push((other, other));
                    added += 1;
                }
            }
        }
        Ok(())
    }

    /// `cclasscvec`: the characters of a class. The classes `ascii`, `blank`, `cntrl` and
    /// `xdigit` are the same in every collation, and the collation gives the others. Under case
    /// folding `lower` and `upper` are `alpha`.
    fn parts(&self, cls: Cls, icase: bool) -> &'c [(char, char)] {
        let class = match cls {
            Cls::Ascii => return &[('\0', '\u{7f}')],
            Cls::Blank => return &[('\t', '\t'), (' ', ' ')],
            Cls::Cntrl => return &[('\0', '\u{1f}'), ('\u{7f}', '\u{9f}')],
            Cls::Xdigit => return &[('0', '9'), ('A', 'F'), ('a', 'f')],
            Cls::Lower | Cls::Upper if icase => PgClass::Alpha,
            Cls::Alnum => PgClass::Alnum,
            Cls::Alpha => PgClass::Alpha,
            Cls::Digit => PgClass::Digit,
            Cls::Graph => PgClass::Graph,
            Cls::Lower => PgClass::Lower,
            Cls::Print => PgClass::Print,
            Cls::Punct => PgClass::Punct,
            Cls::Space => PgClass::Space,
            Cls::Upper => PgClass::Upper,
            Cls::Word => PgClass::Word,
        };
        self.ctype.class(class)
    }
}

fn prefer(greedy: bool) -> u8 {
    if greedy { LONGER } else { SHORTER }
}

/// `element`: the character a collating name stands for.
fn element(name: &str) -> Result<char, Code> {
    let mut chars = name.chars();
    match (chars.next(), chars.next()) {
        (None, _) => Err(Code::Collate),
        (Some(ch), None) => Ok(ch),
        _ => NAMES.iter().find(|(known, _)| *known == name).map(|&(_, ch)| ch).ok_or(Code::Collate),
    }
}

/// The names of `regc_locale.c`, in its order.
const NAMES: [(&str, char); 95] = [
    ("NUL", '\0'),
    ("SOH", '\u{1}'),
    ("STX", '\u{2}'),
    ("ETX", '\u{3}'),
    ("EOT", '\u{4}'),
    ("ENQ", '\u{5}'),
    ("ACK", '\u{6}'),
    ("BEL", '\u{7}'),
    ("alert", '\u{7}'),
    ("BS", '\u{8}'),
    ("backspace", '\u{8}'),
    ("HT", '\t'),
    ("tab", '\t'),
    ("LF", '\n'),
    ("newline", '\n'),
    ("VT", '\u{b}'),
    ("vertical-tab", '\u{b}'),
    ("FF", '\u{c}'),
    ("form-feed", '\u{c}'),
    ("CR", '\r'),
    ("carriage-return", '\r'),
    ("SO", '\u{e}'),
    ("SI", '\u{f}'),
    ("DLE", '\u{10}'),
    ("DC1", '\u{11}'),
    ("DC2", '\u{12}'),
    ("DC3", '\u{13}'),
    ("DC4", '\u{14}'),
    ("NAK", '\u{15}'),
    ("SYN", '\u{16}'),
    ("ETB", '\u{17}'),
    ("CAN", '\u{18}'),
    ("EM", '\u{19}'),
    ("SUB", '\u{1a}'),
    ("ESC", '\u{1b}'),
    ("IS4", '\u{1c}'),
    ("FS", '\u{1c}'),
    ("IS3", '\u{1d}'),
    ("GS", '\u{1d}'),
    ("IS2", '\u{1e}'),
    ("RS", '\u{1e}'),
    ("IS1", '\u{1f}'),
    ("US", '\u{1f}'),
    ("space", ' '),
    ("exclamation-mark", '!'),
    ("quotation-mark", '"'),
    ("number-sign", '#'),
    ("dollar-sign", '$'),
    ("percent-sign", '%'),
    ("ampersand", '&'),
    ("apostrophe", '\''),
    ("left-parenthesis", '('),
    ("right-parenthesis", ')'),
    ("asterisk", '*'),
    ("plus-sign", '+'),
    ("comma", ','),
    ("hyphen", '-'),
    ("hyphen-minus", '-'),
    ("period", '.'),
    ("full-stop", '.'),
    ("slash", '/'),
    ("solidus", '/'),
    ("zero", '0'),
    ("one", '1'),
    ("two", '2'),
    ("three", '3'),
    ("four", '4'),
    ("five", '5'),
    ("six", '6'),
    ("seven", '7'),
    ("eight", '8'),
    ("nine", '9'),
    ("colon", ':'),
    ("semicolon", ';'),
    ("less-than-sign", '<'),
    ("equals-sign", '='),
    ("greater-than-sign", '>'),
    ("question-mark", '?'),
    ("commercial-at", '@'),
    ("left-square-bracket", '['),
    ("backslash", '\\'),
    ("reverse-solidus", '\\'),
    ("right-square-bracket", ']'),
    ("circumflex", '^'),
    ("circumflex-accent", '^'),
    ("underscore", '_'),
    ("low-line", '_'),
    ("grave-accent", '`'),
    ("left-brace", '{'),
    ("left-curly-bracket", '{'),
    ("vertical-line", '|'),
    ("right-brace", '}'),
    ("right-curly-bracket", '}'),
    ("tilde", '~'),
    ("DEL", '\u{7f}'),
];
