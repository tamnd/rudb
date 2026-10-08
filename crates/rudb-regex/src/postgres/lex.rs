//! The lexer, which is `regc_lex.c`.
//!
//! The lexer knows more of the syntax than a lexer usually does, because the meaning of a
//! character depends on where it is: inside a bracket, inside a bound, in a BRE or in an ARE. The
//! context is a state the lexer carries, and the parser moves it only by asking for the next token.
//! The token before the current one is kept as well, because a few rules look back at it, such as
//! a `]` right after the `[` being a character.

use super::{Cflags, Code};

/// A token, which is PostgreSQL's `nexttype` and `nextvalue` together.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Tok {
    /// Nothing has been read yet.
    Empty,
    Eos,
    Plain(char),
    Digit(u32),
    Comma,
    /// The `}` that closes a bound, with whether it prefers the longer match.
    RBrace(bool),
    LBrace,
    Bar,
    Star(bool),
    Plus(bool),
    Quest(bool),
    /// An opening parenthesis, with whether it captures.
    LParen(bool),
    RParen,
    /// An opening bracket, with whether it is not negated.
    LBracket(bool),
    RBracket,
    Dot,
    Caret,
    Dollar,
    /// `\m` and `[[:<:]]`.
    WordStart,
    /// `\M` and `[[:>:]]`.
    WordEnd,
    /// `\A`.
    TextStart,
    /// `\Z`.
    TextEnd,
    /// `\y`.
    Boundary,
    /// `\Y`.
    NotBoundary,
    Look {
        ahead: bool,
        negated: bool,
    },
    /// `\d`, `\s` and `\w`.
    ClassS(Cls),
    /// `\D`, `\S` and `\W`.
    ClassC(Cls),
    Backref(u32),
    /// A `-` inside a bracket that is not at either end of it.
    Range(char),
    /// `[.`
    Collel,
    /// `[=`
    Eclass,
    /// `[:`
    Cclass,
    /// `.]`, `=]` or `:]`.
    End,
}

/// The character classes, in the order of `classNames` in `regc_locale.c`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Cls {
    Alnum,
    Alpha,
    Ascii,
    Blank,
    Cntrl,
    Digit,
    Graph,
    Lower,
    Print,
    Punct,
    Space,
    Upper,
    Xdigit,
    Word,
}

impl Cls {
    pub(super) fn named(name: &str) -> Option<Self> {
        Some(match name {
            "alnum" => Self::Alnum,
            "alpha" => Self::Alpha,
            "ascii" => Self::Ascii,
            "blank" => Self::Blank,
            "cntrl" => Self::Cntrl,
            "digit" => Self::Digit,
            "graph" => Self::Graph,
            "lower" => Self::Lower,
            "print" => Self::Print,
            "punct" => Self::Punct,
            "space" => Self::Space,
            "upper" => Self::Upper,
            "xdigit" => Self::Xdigit,
            "word" => Self::Word,
            _ => return None,
        })
    }
}

/// Where the lexer is, which decides what a character means.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Con {
    Ere,
    Bre,
    Quote,
    EreBound,
    BreBound,
    Bracket,
    Collating,
    Equivalence,
    Class,
}

pub(super) struct Lexer {
    chars: Vec<char>,
    now: usize,
    con: Con,
    pub(super) cflags: Cflags,
    pub(super) next: Tok,
    last: Tok,
    /// How many capturing groups have opened, which decides whether `\12` is a back reference.
    pub(super) groups: u32,
}

type Step = Result<(), Code>;

impl Lexer {
    /// Reads the prefixes and the first token, which is `lexstart`.
    pub(super) fn start(pattern: &str, cflags: Cflags) -> Result<Self, Code> {
        let mut lexer = Self {
            chars: pattern.chars().collect(),
            now: 0,
            con: Con::Ere,
            cflags,
            next: Tok::Empty,
            last: Tok::Empty,
            groups: 0,
        };
        lexer.prefixes()?;
        lexer.con = if lexer.cflags.quote {
            Con::Quote
        } else if lexer.cflags.extended {
            Con::Ere
        } else {
            Con::Bre
        };
        lexer.next = Tok::Empty;
        lexer.advance()?;
        Ok(lexer)
    }

    fn at_end(&self) -> bool {
        self.now >= self.chars.len()
    }

    /// The character `by` places on, if there is one.
    fn peek(&self, by: usize) -> Option<char> {
        self.chars.get(self.now + by).copied()
    }

    fn sees(&self, ch: char) -> bool {
        self.peek(0) == Some(ch)
    }

    /// The `***` directors and the embedded options, which is `prefixes`.
    fn prefixes(&mut self) -> Step {
        if self.cflags.quote {
            return Ok(());
        }
        if self.chars.len() >= 4 && self.chars[..3] == ['*', '*', '*'] {
            match self.chars[3] {
                '?' => return Err(Code::BadPat),
                '=' => {
                    self.cflags.quote = true;
                    self.cflags.extended = false;
                    self.cflags.advf = false;
                    self.cflags.expanded = false;
                    self.cflags.nlstop = false;
                    self.cflags.nlanch = false;
                    self.now += 4;
                    return Ok(());
                }
                ':' => {
                    self.cflags.extended = true;
                    self.cflags.advf = true;
                    self.now += 4;
                }
                _ => return Err(Code::BadRpt),
            }
        }
        if !(self.cflags.extended && self.cflags.advf) {
            return Ok(());
        }
        if !(self.sees('(') && self.peek(1) == Some('?'))
            || !self.peek(2).is_some_and(|ch| ch.is_ascii_alphabetic())
        {
            return Ok(());
        }
        self.now += 2;
        while let Some(letter) = self.peek(0).filter(char::is_ascii_alphabetic) {
            let c = &mut self.cflags;
            match letter {
                'b' => {
                    c.extended = false;
                    c.advf = false;
                    c.quote = false;
                }
                'c' => c.icase = false,
                'e' => {
                    c.extended = true;
                    c.advf = false;
                    c.quote = false;
                }
                'i' => c.icase = true,
                'm' | 'n' => {
                    c.nlstop = true;
                    c.nlanch = true;
                }
                'p' => {
                    c.nlstop = true;
                    c.nlanch = false;
                }
                'q' => {
                    c.quote = true;
                    c.extended = false;
                    c.advf = false;
                }
                's' => {
                    c.nlstop = false;
                    c.nlanch = false;
                }
                't' => c.expanded = false,
                'w' => {
                    c.nlstop = false;
                    c.nlanch = true;
                }
                'x' => c.expanded = true,
                _ => return Err(Code::BadOpt),
            }
            self.now += 1;
        }
        if !self.sees(')') {
            return Err(Code::BadOpt);
        }
        self.now += 1;
        if self.cflags.quote {
            self.cflags.expanded = false;
            self.cflags.nlstop = false;
            self.cflags.nlanch = false;
        }
        Ok(())
    }

    fn ret(&mut self, tok: Tok) -> Step {
        self.next = tok;
        Ok(())
    }

    /// Reads the next token, which is `next`.
    pub(super) fn advance(&mut self) -> Step {
        loop {
            self.last = self.next;
            if self.cflags.expanded
                && matches!(self.con, Con::Ere | Con::Bre | Con::EreBound | Con::BreBound)
            {
                self.skip();
            }
            let Some(&c) = self.chars.get(self.now) else {
                return match self.con {
                    Con::Ere | Con::Bre | Con::Quote => self.ret(Tok::Eos),
                    Con::EreBound | Con::BreBound => Err(Code::Brace),
                    _ => Err(Code::Brack),
                };
            };
            self.now += 1;
            match self.con {
                Con::Bre => return self.bre(c),
                Con::Ere => {}
                Con::Quote => return self.ret(Tok::Plain(c)),
                Con::EreBound | Con::BreBound => return self.bound(c),
                Con::Bracket => return self.bracket(c),
                Con::Collating => return self.close(c, '.'),
                Con::Equivalence => return self.close(c, '='),
                Con::Class => return self.close(c, ':'),
            }
            let advf = self.cflags.advf;
            let tok = match c {
                '|' => Tok::Bar,
                '*' | '+' | '?' => {
                    let lazy = advf && self.sees('?');
                    if lazy {
                        self.now += 1;
                    }
                    match c {
                        '*' => Tok::Star(!lazy),
                        '+' => Tok::Plus(!lazy),
                        _ => Tok::Quest(!lazy),
                    }
                }
                '{' => {
                    if self.cflags.expanded {
                        self.skip();
                    }
                    if self.peek(0).is_some_and(|ch| ch.is_ascii_digit()) {
                        self.con = Con::EreBound;
                        Tok::LBrace
                    } else {
                        Tok::Plain(c)
                    }
                }
                '(' => {
                    if !(advf && self.sees('?')) {
                        return self.ret(Tok::LParen(true));
                    }
                    self.now += 1;
                    let Some(kind) = self.peek(0) else { return Err(Code::BadRpt) };
                    self.now += 1;
                    match kind {
                        ':' => Tok::LParen(false),
                        '#' => {
                            while !self.at_end() && !self.sees(')') {
                                self.now += 1;
                            }
                            if !self.at_end() {
                                self.now += 1;
                            }
                            continue;
                        }
                        '=' => Tok::Look { ahead: true, negated: false },
                        '!' => Tok::Look { ahead: true, negated: true },
                        '<' => {
                            let Some(kind) = self.peek(0) else { return Err(Code::BadRpt) };
                            self.now += 1;
                            match kind {
                                '=' => Tok::Look { ahead: false, negated: false },
                                '!' => Tok::Look { ahead: false, negated: true },
                                _ => return Err(Code::BadRpt),
                            }
                        }
                        _ => return Err(Code::BadRpt),
                    }
                }
                ')' => Tok::RParen,
                '[' => return self.open_bracket(),
                '.' => Tok::Dot,
                '^' => Tok::Caret,
                '$' => Tok::Dollar,
                '\\' => {
                    let Some(next) = self.peek(0) else { return Err(Code::Escape) };
                    if !advf {
                        self.now += 1;
                        Tok::Plain(next)
                    } else {
                        self.escape()?
                    }
                }
                _ => Tok::Plain(c),
            };
            return self.ret(tok);
        }
    }

    /// A `[` in ERE or BRE, which may be the word constraints `[[:<:]]` and `[[:>:]]`.
    fn open_bracket(&mut self) -> Step {
        if self.chars.len() >= self.now + 6
            && self.chars[self.now] == '['
            && self.chars[self.now + 1] == ':'
            && matches!(self.chars[self.now + 2], '<' | '>')
            && self.chars[self.now + 3] == ':'
            && self.chars[self.now + 4] == ']'
            && self.chars[self.now + 5] == ']'
        {
            let start = self.chars[self.now + 2] == '<';
            self.now += 6;
            return self.ret(if start { Tok::WordStart } else { Tok::WordEnd });
        }
        self.con = Con::Bracket;
        if self.sees('^') {
            self.now += 1;
            return self.ret(Tok::LBracket(false));
        }
        self.ret(Tok::LBracket(true))
    }

    fn bound(&mut self, c: char) -> Step {
        let tok = match c {
            '0'..='9' => Tok::Digit(c as u32 - '0' as u32),
            ',' => Tok::Comma,
            '}' if self.con == Con::EreBound => {
                self.con = Con::Ere;
                if self.cflags.advf && self.sees('?') {
                    self.now += 1;
                    Tok::RBrace(false)
                } else {
                    Tok::RBrace(true)
                }
            }
            '\\' if self.con == Con::BreBound && self.sees('}') => {
                self.now += 1;
                self.con = Con::Bre;
                Tok::RBrace(true)
            }
            _ => return Err(Code::BadBr),
        };
        self.ret(tok)
    }

    fn bracket(&mut self, c: char) -> Step {
        let opened = matches!(self.last, Tok::LBracket(_));
        let tok = match c {
            ']' if opened => Tok::Plain(c),
            ']' => {
                self.con = if self.cflags.extended { Con::Ere } else { Con::Bre };
                Tok::RBracket
            }
            '\\' => {
                if !self.cflags.advf {
                    return self.ret(Tok::Plain(c));
                }
                if self.at_end() {
                    return Err(Code::Escape);
                }
                let tok = self.escape()?;
                if !matches!(tok, Tok::Plain(_) | Tok::ClassS(_) | Tok::ClassC(_)) {
                    return Err(Code::Escape);
                }
                tok
            }
            '-' if opened || self.sees(']') => Tok::Plain(c),
            '-' => Tok::Range(c),
            '[' => {
                let Some(kind) = self.peek(0) else { return Err(Code::Brack) };
                self.now += 1;
                match kind {
                    '.' => {
                        self.con = Con::Collating;
                        Tok::Collel
                    }
                    '=' => {
                        self.con = Con::Equivalence;
                        Tok::Eclass
                    }
                    ':' => {
                        self.con = Con::Class;
                        Tok::Cclass
                    }
                    _ => {
                        self.now -= 1;
                        Tok::Plain(c)
                    }
                }
            }
            _ => Tok::Plain(c),
        };
        self.ret(tok)
    }

    /// Inside `[.`, `[=` or `[:`, where only the closing pair is not a character.
    fn close(&mut self, c: char, mark: char) -> Step {
        if c == mark && self.sees(']') {
            self.now += 1;
            self.con = Con::Bracket;
            return self.ret(Tok::End);
        }
        self.ret(Tok::Plain(c))
    }

    /// A BRE token, which is `brenext`.
    fn bre(&mut self, c: char) -> Step {
        let tok = match c {
            '*' => {
                if matches!(self.last, Tok::Empty | Tok::LParen(_) | Tok::Caret) {
                    Tok::Plain(c)
                } else {
                    Tok::Star(true)
                }
            }
            '[' => return self.open_bracket(),
            '.' => Tok::Dot,
            '^' => {
                if matches!(self.last, Tok::Empty | Tok::LParen(_)) {
                    Tok::Caret
                } else {
                    Tok::Plain(c)
                }
            }
            '$' => {
                if self.cflags.expanded {
                    self.skip();
                }
                if self.at_end() || (self.sees('\\') && self.peek(1) == Some(')')) {
                    Tok::Dollar
                } else {
                    Tok::Plain(c)
                }
            }
            '\\' => {
                let Some(next) = self.peek(0) else { return Err(Code::Escape) };
                self.now += 1;
                match next {
                    '{' => {
                        self.con = Con::BreBound;
                        Tok::LBrace
                    }
                    '(' => Tok::LParen(true),
                    ')' => Tok::RParen,
                    '<' => Tok::WordStart,
                    '>' => Tok::WordEnd,
                    '1'..='9' => Tok::Backref(next as u32 - '0' as u32),
                    _ => Tok::Plain(next),
                }
            }
            _ => Tok::Plain(c),
        };
        self.ret(tok)
    }

    /// An ARE escape, with the backslash already read, which is `lexescape`.
    fn escape(&mut self) -> Result<Tok, Code> {
        let Some(c) = self.peek(0) else { return Err(Code::Escape) };
        self.now += 1;
        if !c.is_ascii_alphanumeric() {
            return Ok(Tok::Plain(c));
        }
        Ok(match c {
            'a' => Tok::Plain('\u{7}'),
            'A' => Tok::TextStart,
            'b' => Tok::Plain('\u{8}'),
            'B' => Tok::Plain('\\'),
            'c' => {
                let Some(next) = self.peek(0) else { return Err(Code::Escape) };
                self.now += 1;
                Tok::Plain(char::from_u32(next as u32 & 0o37).ok_or(Code::Escape)?)
            }
            'd' => Tok::ClassS(Cls::Digit),
            'D' => Tok::ClassC(Cls::Digit),
            'e' => Tok::Plain('\u{1b}'),
            'f' => Tok::Plain('\u{c}'),
            'm' => Tok::WordStart,
            'M' => Tok::WordEnd,
            'n' => Tok::Plain('\n'),
            'r' => Tok::Plain('\r'),
            's' => Tok::ClassS(Cls::Space),
            'S' => Tok::ClassC(Cls::Space),
            't' => Tok::Plain('\t'),
            'u' => Tok::Plain(self.code(4, 4)?),
            'U' => Tok::Plain(self.code(8, 8)?),
            'v' => Tok::Plain('\u{b}'),
            'w' => Tok::ClassS(Cls::Word),
            'W' => Tok::ClassC(Cls::Word),
            'x' => Tok::Plain(self.code(1, 255)?),
            'y' => Tok::Boundary,
            'Y' => Tok::NotBoundary,
            'Z' => Tok::TextEnd,
            '1'..='9' => {
                let save = self.now;
                self.now -= 1;
                let number = self.digits(10, 1, 255)?;
                // The heuristic is PostgreSQL's: one digit is always a back reference, and more
                // than one is one only when that many groups have opened.
                if self.now == save || (number > 0 && number <= self.groups) {
                    return Ok(Tok::Backref(number));
                }
                self.now = save;
                self.octal()?
            }
            '0' => self.octal()?,
            _ => return Err(Code::Escape),
        })
    }

    /// An octal escape with its first digit already read.
    fn octal(&mut self) -> Result<Tok, Code> {
        self.now -= 1;
        let mut number = self.digits(8, 1, 3)?;
        if number > 0xff {
            self.now -= 1;
            number >>= 3;
        }
        Ok(Tok::Plain(char::from_u32(number).ok_or(Code::Escape)?))
    }

    /// A hexadecimal code point of between `least` and `most` digits.
    fn code(&mut self, least: usize, most: usize) -> Result<char, Code> {
        let number = self.digits(16, least, most)?;
        char::from_u32(number).ok_or(Code::Escape)
    }

    /// Reads up to `most` digits in a base, which is `lexdigits`. The arithmetic wraps the way the
    /// C does on its unsigned type.
    fn digits(&mut self, base: u32, least: usize, most: usize) -> Result<u32, Code> {
        let mut number = 0u32;
        let mut read = 0;
        while read < most {
            let Some(digit) = self.peek(0).and_then(|ch| ch.to_digit(16)).filter(|&d| d < base)
            else {
                break;
            };
            self.now += 1;
            number = number.wrapping_mul(base).wrapping_add(digit);
            read += 1;
        }
        if read < least {
            return Err(Code::Escape);
        }
        Ok(number)
    }

    /// Steps over white space and `#` comments, for the expanded syntax.
    fn skip(&mut self) {
        loop {
            while self.peek(0).is_some_and(char::is_whitespace) {
                self.now += 1;
            }
            if !self.sees('#') {
                return;
            }
            while !self.at_end() && !self.sees('\n') {
                self.now += 1;
            }
        }
    }

    /// The characters of a `[.name.]`, `[=name=]` or `[:name:]` up to the closing pair, with the
    /// token after it read, which is `scanplain`.
    pub(super) fn name(&mut self) -> Result<String, Code> {
        self.advance()?;
        let mut name = String::new();
        while let Tok::Plain(ch) = self.next {
            name.push(ch);
            self.advance()?;
        }
        self.advance()?;
        Ok(name)
    }
}
