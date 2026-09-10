//! Reading DuckDB's PEG grammar into something a generator can walk.
//!
//! This is a parser for the meta-language, not for SQL. The dialect is small and it is fully
//! inventoried: 1,087 rule arrows across forty files, ordered choice, sequence, the three
//! postfix operators, grouping, two parameterized rules, one negative lookahead, two captures,
//! nine character classes and nothing else. `spec/20-the-grammar.md` section 2 has the survey
//! and the counts, and the tests at the bottom of this file are that survey turned into
//! assertions, so a bump that introduces a construct we do not handle fails the build instead of
//! quietly dropping a rule.
//!
//! The one part that is not obvious from the syntax is where a rule ends. Newlines are not
//! terminators, and plenty of rules put each alternative on its own line, so a sequence ends when
//! the next thing in the file is a name followed by an arrow. That is the same rule cpp-peglib
//! uses and it is why `Statement` can be thirty six lines long without any continuation marker.

use std::path::Path;

/// One rule, in the order it appears in the file it came from.
#[derive(Debug, Clone)]
pub(crate) struct Rule {
    pub(crate) name: String,
    /// The single parameter of a parameterized rule, being `D` in `List(D)` and in `Parens(D)`.
    /// Upstream rejects more than one, so there is no reason to carry a vector here.
    pub(crate) parameter: Option<String>,
    pub(crate) body: Expr,
    /// The file it was read from, for error messages that name something a person can open.
    pub(crate) origin: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Expr {
    /// `'SELECT'`, `','`, `'::'`. Case insensitive, and the tokenizer has already decided what a
    /// token is, so this is a comparison and never a scan.
    Literal(String),
    /// A rule name, or the parameter of a parameterized rule.
    Reference(String),
    /// `List(X)` or `Parens(X)`.
    Call(String, Box<Expr>),
    Sequence(Vec<Expr>),
    Choice(Vec<Expr>),
    /// `X?`
    Optional(Box<Expr>),
    /// `X+`, being one or more. `X*` is parsed as `Optional(Repeat(X))`, which is how upstream
    /// builds it, so zero or more is not a separate node anywhere downstream.
    Repeat(Box<Expr>),
    /// `!X`. There is exactly one in the grammar and it is in a rule that the matcher overrides,
    /// so it is unreachable. Kept as a node rather than dropped, because a second one appearing
    /// upstream is something the generator has to be able to say out loud.
    Not(Box<Expr>),
    /// `< X >`, which marks the captured text of a token level rule. Two of them, both in rules
    /// the matcher overrides.
    Capture(Box<Expr>),
    /// `[a-z_]i` and its seven friends. Same story: token level, and overridden.
    CharClass(String),
}

/// Reads every `.gram` file in a directory, in file name order.
///
/// File order is fixed rather than whatever the filesystem hands back, because the generated
/// output is checked in and compared byte for byte, and a table that reorders itself depending on
/// which machine ran the generator is a table that fails the gate at random.
pub(crate) fn parse_dir(dir: &Path) -> Result<Vec<Rule>, String> {
    let mut files: Vec<_> = std::fs::read_dir(dir)
        .map_err(|e| format!("could not read {}: {e}", dir.display()))?
        .filter_map(Result::ok)
        .map(|entry| entry.path())
        .filter(|path| path.extension().and_then(|e| e.to_str()) == Some("gram"))
        .collect();
    files.sort();
    if files.is_empty() {
        return Err(format!("{} has no .gram files in it", dir.display()));
    }

    let mut rules = Vec::new();
    for path in files {
        let text = std::fs::read_to_string(&path)
            .map_err(|e| format!("could not read {}: {e}", path.display()))?;
        let name = path.file_name().unwrap_or_default().to_string_lossy().into_owned();
        rules.extend(parse(&text, &name)?);
    }
    Ok(rules)
}

/// Reads one file.
pub(crate) fn parse(text: &str, origin: &str) -> Result<Vec<Rule>, String> {
    Parser { bytes: text.as_bytes(), text, at: 0, origin }.file()
}

struct Parser<'a> {
    bytes: &'a [u8],
    text: &'a str,
    at: usize,
    origin: &'a str,
}

impl<'a> Parser<'a> {
    fn file(mut self) -> Result<Vec<Rule>, String> {
        let mut rules = Vec::new();
        self.space();
        while self.at < self.bytes.len() {
            rules.push(self.rule()?);
            self.space();
        }
        Ok(rules)
    }

    fn rule(&mut self) -> Result<Rule, String> {
        let name = self.name().ok_or_else(|| self.error("expected a rule name"))?;
        self.space();
        let mut parameter = None;
        if self.peek() == Some(b'(') {
            self.at += 1;
            self.space();
            parameter = Some(self.name().ok_or_else(|| self.error("expected a parameter name"))?);
            self.space();
            self.expect(b')')?;
            self.space();
        }
        if !self.take("<-") {
            return Err(self.error(&format!("expected `<-` after {name}")));
        }
        let body = self.choice()?;
        Ok(Rule { name, parameter, body, origin: self.origin.to_string() })
    }

    fn choice(&mut self) -> Result<Expr, String> {
        let mut branches = vec![self.sequence()?];
        loop {
            self.space();
            if self.peek() != Some(b'/') {
                break;
            }
            self.at += 1;
            branches.push(self.sequence()?);
        }
        Ok(if branches.len() == 1 {
            branches.pop().expect("one branch")
        } else {
            Expr::Choice(branches)
        })
    }

    fn sequence(&mut self) -> Result<Expr, String> {
        let mut items = Vec::new();
        loop {
            self.space();
            if self.at_rule_start() {
                break;
            }
            match self.peek() {
                None | Some(b'/') | Some(b')') | Some(b'>') => break,
                _ => {}
            }
            items.push(self.prefixed()?);
        }
        if items.is_empty() {
            return Err(self.error("expected an expression"));
        }
        Ok(if items.len() == 1 { items.pop().expect("one item") } else { Expr::Sequence(items) })
    }

    fn prefixed(&mut self) -> Result<Expr, String> {
        if self.peek() == Some(b'!') {
            self.at += 1;
            self.space();
            return Ok(Expr::Not(Box::new(self.suffixed()?)));
        }
        self.suffixed()
    }

    fn suffixed(&mut self) -> Result<Expr, String> {
        let inner = self.primary()?;
        // No space skip before the suffix. `A ?` is not a thing in this grammar and treating it as
        // one would let a missing operator read as an optional element two lines further down.
        match self.peek() {
            Some(b'?') => {
                self.at += 1;
                Ok(Expr::Optional(Box::new(inner)))
            }
            Some(b'+') => {
                self.at += 1;
                Ok(Expr::Repeat(Box::new(inner)))
            }
            Some(b'*') => {
                self.at += 1;
                Ok(Expr::Optional(Box::new(Expr::Repeat(Box::new(inner)))))
            }
            _ => Ok(inner),
        }
    }

    fn primary(&mut self) -> Result<Expr, String> {
        match self.peek() {
            Some(b'\'') => self.literal(),
            Some(b'[') => self.char_class(),
            Some(b'(') => {
                self.at += 1;
                let inner = self.choice()?;
                self.space();
                self.expect(b')')?;
                Ok(inner)
            }
            Some(b'<') => {
                self.at += 1;
                let inner = self.choice()?;
                self.space();
                self.expect(b'>')?;
                Ok(Expr::Capture(Box::new(inner)))
            }
            _ => {
                let name = self.name().ok_or_else(|| self.error("expected a rule reference"))?;
                // No space skip here either. `List (X)` does not appear and `Foo (Bar / Baz)` as a
                // reference followed by a group does, so the paren has to be touching to be a call.
                if self.peek() == Some(b'(') {
                    self.at += 1;
                    let argument = self.choice()?;
                    self.space();
                    self.expect(b')')?;
                    return Ok(Expr::Call(name, Box::new(argument)));
                }
                Ok(Expr::Reference(name))
            }
        }
    }

    fn literal(&mut self) -> Result<Expr, String> {
        self.at += 1;
        let mut out = String::new();
        loop {
            match self.peek() {
                None => return Err(self.error("unterminated literal")),
                Some(b'\'') => {
                    self.at += 1;
                    return Ok(Expr::Literal(out));
                }
                Some(b'\\') => {
                    self.at += 1;
                    match self.peek() {
                        None => return Err(self.error("unterminated escape")),
                        Some(byte) => {
                            out.push(byte as char);
                            self.at += 1;
                        }
                    }
                }
                Some(byte) => {
                    out.push(byte as char);
                    self.at += 1;
                }
            }
        }
    }

    fn char_class(&mut self) -> Result<Expr, String> {
        let start = self.at;
        self.at += 1;
        loop {
            match self.peek() {
                None => return Err(self.error("unterminated character class")),
                Some(b']') => {
                    self.at += 1;
                    break;
                }
                Some(b'\\') => self.at += 2,
                Some(_) => self.at += 1,
            }
        }
        // The `i` suffix is case insensitivity. Kept in the text rather than as a flag, because
        // nothing downstream reads a character class: they only exist in overridden rules and the
        // generator's job is to notice if that ever stops being true.
        if self.peek() == Some(b'i') {
            self.at += 1;
        }
        Ok(Expr::CharClass(self.text[start..self.at].to_string()))
    }

    /// Whether what comes next is the start of a new rule rather than another element.
    ///
    /// This is the whole reason the parser needs lookahead. `Statement <- A /\n B` and
    /// `Foo <- A\nBar <- B` differ only in what follows the name on the next line.
    fn at_rule_start(&self) -> bool {
        let mut probe = Parser { bytes: self.bytes, text: self.text, at: self.at, origin: "" };
        if probe.name().is_none() {
            return false;
        }
        probe.space();
        if probe.peek() == Some(b'(') {
            probe.at += 1;
            probe.space();
            if probe.name().is_none() {
                return false;
            }
            probe.space();
            if probe.peek() != Some(b')') {
                return false;
            }
            probe.at += 1;
            probe.space();
        }
        probe.take("<-")
    }

    fn name(&mut self) -> Option<String> {
        let start = self.at;
        if self.peek() == Some(b'%') {
            self.at += 1;
        }
        while matches!(self.peek(), Some(byte) if byte.is_ascii_alphanumeric() || byte == b'_') {
            self.at += 1;
        }
        // A lone `%` is not a name, and neither is nothing at all.
        let name = &self.text[start..self.at];
        if name.is_empty() || name == "%" {
            self.at = start;
            return None;
        }
        Some(name.to_string())
    }

    fn space(&mut self) {
        loop {
            match self.peek() {
                Some(byte) if byte.is_ascii_whitespace() => self.at += 1,
                Some(b'#') => {
                    while !matches!(self.peek(), None | Some(b'\n')) {
                        self.at += 1;
                    }
                }
                _ => return,
            }
        }
    }

    fn peek(&self) -> Option<u8> {
        self.bytes.get(self.at).copied()
    }

    fn take(&mut self, word: &str) -> bool {
        if self.text[self.at..].starts_with(word) {
            self.at += word.len();
            true
        } else {
            false
        }
    }

    fn expect(&mut self, byte: u8) -> Result<(), String> {
        if self.peek() == Some(byte) {
            self.at += 1;
            Ok(())
        } else {
            Err(self.error(&format!("expected `{}`", byte as char)))
        }
    }

    fn error(&self, message: &str) -> String {
        let line = self.text[..self.at].bytes().filter(|byte| *byte == b'\n').count() + 1;
        format!("{}:{line}: {message}", self.origin)
    }
}

#[cfg(test)]
mod tests {
    use super::{Expr, parse, parse_dir};

    fn one(text: &str) -> Expr {
        let rules = parse(text, "test").expect("the rule does not parse");
        assert_eq!(rules.len(), 1, "expected one rule, got {}", rules.len());
        rules.into_iter().next().expect("one rule").body
    }

    #[test]
    fn a_sequence_ends_where_the_next_rule_begins() {
        let rules = parse("Foo <- A B\nBar <- C\n", "test").expect("does not parse");
        assert_eq!(rules.len(), 2);
        assert_eq!(rules[0].body, Expr::Sequence(vec![reference("A"), reference("B")]));
        assert_eq!(rules[1].body, reference("C"));
    }

    #[test]
    fn a_choice_carries_on_across_lines() {
        let rules = parse("Foo <-\n\tA /\n\tB /\n\tC\nBar <- D\n", "test").expect("does not parse");
        assert_eq!(rules.len(), 2);
        assert_eq!(
            rules[0].body,
            Expr::Choice(vec![reference("A"), reference("B"), reference("C")])
        );
    }

    #[test]
    fn star_is_optional_around_repeat() {
        // Upstream builds `X*` as Optional(Repeat(X)) in matcher_factory.cpp, so zero or more is
        // not a node kind anywhere. Keeping the same shape means the two can be compared.
        assert_eq!(
            one("Foo <- A*\n"),
            Expr::Optional(Box::new(Expr::Repeat(Box::new(reference("A")))))
        );
        assert_eq!(one("Foo <- A+\n"), Expr::Repeat(Box::new(reference("A"))));
        assert_eq!(one("Foo <- A?\n"), Expr::Optional(Box::new(reference("A"))));
    }

    #[test]
    fn a_call_needs_the_paren_touching_the_name() {
        assert_eq!(one("Foo <- List(A)\n"), Expr::Call("List".into(), Box::new(reference("A"))));
        assert_eq!(
            one("Foo <- A (B / C)\n"),
            Expr::Sequence(vec![
                reference("A"),
                Expr::Choice(vec![reference("B"), reference("C")])
            ])
        );
    }

    #[test]
    fn a_literal_keeps_its_escaped_quote() {
        assert_eq!(one("Foo <- '\\''\n"), Expr::Literal("'".into()));
        assert_eq!(one("Foo <- ','\n"), Expr::Literal(",".into()));
    }

    #[test]
    fn a_comment_is_not_part_of_a_rule() {
        let rules =
            parse("# a comment with an apostrophe: don't\nFoo <- A # trailing\nBar <- B\n", "test")
                .expect("does not parse");
        assert_eq!(rules.len(), 2);
        assert_eq!(rules[0].body, reference("A"));
    }

    /// The vendored grammar is in the tree and the gate already checks it is upstream's, so this
    /// parses the real thing rather than a fixture that agrees with whatever the parser does.
    #[test]
    fn the_vendored_grammar_parses_whole() {
        let dir = crate::root().join(crate::vendor::DEST).join("statements");
        let rules = parse_dir(&dir).expect("the vendored grammar does not parse");
        // 1,087 arrows at v2.0. Exact rather than a floor: this number is the one thing that says
        // the parser read all of it, and a bump that changes it should be looked at rather than
        // absorbed.
        assert_eq!(rules.len(), 1087, "the grammar has a different number of rules");

        let parameterized: Vec<&str> = rules
            .iter()
            .filter(|rule| rule.parameter.is_some())
            .map(|rule| rule.name.as_str())
            .collect();
        assert_eq!(parameterized, ["List", "Parens"]);

        let mut lookaheads = 0;
        let mut captures = 0;
        let mut classes = 0;
        for rule in &rules {
            walk(&rule.body, &mut |expr| match expr {
                Expr::Not(_) => lookaheads += 1,
                Expr::Capture(_) => captures += 1,
                Expr::CharClass(_) => classes += 1,
                _ => {}
            });
        }
        assert_eq!(
            lookaheads, 1,
            "the survey in spec/20-the-grammar.md says one negative lookahead"
        );
        assert_eq!(captures, 2, "the survey says two captures");
        // Nine, all of them inside the five token level rules the matcher overrides: four in
        // `NumberLiteral`, two in `PlainIdentifier`, and one each in `StringLiteral`,
        // `QuotedIdentifier` and `%whitespace`. A tenth appearing somewhere reachable would mean
        // the grammar had started asking us to scan bytes, which the tokenizer has already done.
        assert_eq!(classes, 9, "the survey says nine character classes");
    }

    fn reference(name: &str) -> Expr {
        Expr::Reference(name.into())
    }

    fn walk(expr: &Expr, visit: &mut impl FnMut(&Expr)) {
        visit(expr);
        match expr {
            Expr::Sequence(children) | Expr::Choice(children) => {
                for child in children {
                    walk(child, visit);
                }
            }
            Expr::Optional(child)
            | Expr::Repeat(child)
            | Expr::Not(child)
            | Expr::Capture(child)
            | Expr::Call(_, child) => walk(child, visit),
            Expr::Literal(_) | Expr::Reference(_) | Expr::CharClass(_) => {}
        }
    }
}
