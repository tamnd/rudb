//! The tokenizer, matched to DuckDB's by behaviour.
//!
//! This is the one part of the front end with no declarative artifact behind it. The grammar is
//! vendored and generated from, per `spec/20-the-grammar.md`, and it says nothing about string
//! literals, dollar quoting, numeric literal forms, comments or the operator rules. All of that
//! is 613 lines of hand written C++ upstream, in `src/parser/peg/tokenizer/base_tokenizer.cpp`
//! and `parser_tokenizer.cpp`, and this is a port of it rather than an interpretation.
//!
//! Which is worth saying plainly: everything here is a fact about DuckDB and not a design
//! decision of ours. Where the behaviour looks wrong, it is still the behaviour, because a query
//! that returns a different answer is worse than a query that returns a surprising one. Section
//! 20.7 enumerates the ten that bite. The reason a differential fuzzer against a real DuckDB is
//! scheduled from week one rather than at M5 is this file.
//!
//! One deliberate divergence, and it is representational. Upstream types a quoted identifier and
//! a bare one both as `IDENTIFIER` and recovers the difference from the first byte where it
//! matters. We give them separate kinds. Nothing downstream may treat them differently in a place
//! upstream does not.
//!
//! Nothing is decoded. A string keeps its quotes and its escapes, a number keeps its underscores,
//! an identifier keeps its case. `spec/20-the-grammar.md` section 7 for why case in particular:
//! DuckDB is case insensitive and case preserving, including for quoted identifiers, which is not
//! what PostgreSQL does and not what folding here would give.

use rudb_common::{Error, Result, Span};

use crate::generated::keywords::{KEYWORDS, LONGEST};
use crate::token::{Flags, Kind, NOT_A_KEYWORD, Token};

/// Split `query` into tokens, ending with exactly one [`Kind::EndOfInput`].
///
/// Fails only where DuckDB's parser tokenizer throws, which is four cases: an unterminated block
/// comment, an unterminated string literal, an unterminated quoted identifier, and the empty
/// quoted identifier `""`. An unterminated dollar quoted string is not one of them and comes back
/// as a token with [`Flags::UNTERMINATED`] set.
pub fn tokenize(query: &str) -> Result<Vec<Token>> {
    Tokenizer::new(query).run()
}

/// Where the scan is.
///
/// Named for what upstream calls them so that reading the two side by side stays possible, which
/// matters more here than anywhere else in the crate.
#[derive(Clone, Copy, PartialEq, Eq)]
enum State {
    Standard,
    LineComment,
    BlockComment,
    QuotedIdentifier,
    StringLiteral,
    Word,
    Numeric,
    Operator,
    DollarQuoted,
}

struct Tokenizer<'a> {
    query: &'a str,
    bytes: &'a [u8],
    tokens: Vec<Token>,
    /// Where the token being built starts, and after a token is pushed, where the gap starts.
    last: usize,
    /// A block comment ended here. Used to set [`Flags::BLOCK_COMMENT`] on the next token, which
    /// is the whole reason comments are tracked rather than skipped.
    block_comment_at: Option<usize>,
    /// Set by an `E` prefix, which is the only one of the four that changes how the body is read.
    escape_string: bool,
    /// The tag between the dollars, as a span, so that closing it is a slice comparison.
    dollar_tag: Span,
    depth: u32,
}

impl<'a> Tokenizer<'a> {
    fn new(query: &'a str) -> Self {
        Tokenizer {
            query,
            bytes: query.as_bytes(),
            // Two tokens every seven bytes is what the ClickBench and TPC-H queries come out at.
            // Being wrong here costs a realloc, being absent costs six.
            tokens: Vec::with_capacity(query.len() / 4 + 4),
            last: 0,
            block_comment_at: None,
            escape_string: false,
            dollar_tag: Span::new(0, 0),
            depth: 0,
        }
    }

    fn run(mut self) -> Result<Vec<Token>> {
        let mut state = State::Standard;
        let mut i = 0;
        while i < self.bytes.len() {
            let c = self.bytes[i];
            match state {
                State::Standard => {
                    if let Some(next) = self.standard(&mut i, c)? {
                        state = next;
                    }
                }
                State::Numeric => self.numeric(&mut state, &mut i, c),
                State::Operator => self.operator(&mut state, &mut i, c),
                State::Word => self.word(&mut state, &mut i, c),
                State::StringLiteral => self.string_literal(&mut state, &mut i, c),
                State::QuotedIdentifier => self.quoted_identifier(&mut state, &mut i, c)?,
                State::LineComment => {
                    if c == b'\n' || c == b'\r' {
                        self.comment(self.last, i + 1);
                        self.last = i + 1;
                        state = State::Standard;
                    }
                }
                State::BlockComment => self.block_comment(&mut state, &mut i, c),
                State::DollarQuoted => self.dollar_quoted(&mut state, &mut i),
            }
            i += 1;
        }
        self.finish(state)
    }

    /// The end of the input, which is a different decision per state and not a loop exit.
    fn finish(mut self, state: State) -> Result<Vec<Token>> {
        let end = self.bytes.len();
        match state {
            State::LineComment => {
                self.comment(self.last, end);
            }
            State::BlockComment => {
                return Err(self.error(
                    format!(
                        "unterminated /* comment at or near \"{}\"",
                        &self.query[self.last..end]
                    ),
                    self.last,
                ));
            }
            State::Operator => self.push_operator(self.last, end),
            State::DollarQuoted => {
                // Not an error upstream, which is worth noticing rather than tidying up. It comes
                // back as a token that ran off the end and the grammar gets to decide.
                self.push_flagged(self.last, end, Kind::String, Flags::UNTERMINATED);
            }
            State::StringLiteral => {
                return Err(self.error("unterminated string literal", self.last));
            }
            State::QuotedIdentifier => {
                return Err(self.error("unterminated quoted identifier", self.last));
            }
            State::Numeric => self.push(self.last, end, Kind::Number),
            State::Word => self.push_word(self.last, end),
            // A `$` with nothing after it lands here, and upstream calls it an identifier. It is
            // reproduced rather than corrected, because a tokenizer that disagrees with DuckDB
            // about a one byte query disagrees with it about something.
            State::Standard => self.push(self.last, end, Kind::Identifier),
        }
        self.tokens.push(Token {
            kind: Kind::EndOfInput,
            flags: Flags::default(),
            keyword: NOT_A_KEYWORD,
            start: end as u32,
            end: end as u32,
        });
        Ok(self.tokens)
    }

    /// The dispatch at the top of a token. Returns the state to move to, if it changes.
    fn standard(&mut self, i: &mut usize, c: u8) -> Result<Option<State>> {
        match c {
            b'\'' => {
                self.last = *i;
                self.escape_string = false;
                return Ok(Some(State::StringLiteral));
            }
            b'"' => {
                self.last = *i;
                return Ok(Some(State::QuotedIdentifier));
            }
            b';' => {
                // The base tokenizer emits nothing here and lets its caller decide. The parser's
                // caller emits `;`, because `Program <- TopLevelStatement*` consumes it. The
                // autocomplete caller does something else, which is why the hook exists.
                self.tokens.push(Token {
                    kind: Kind::Terminator,
                    flags: self.gap_flags(*i),
                    keyword: NOT_A_KEYWORD,
                    start: *i as u32,
                    end: *i as u32 + 1,
                });
                self.last = *i + 1;
                return Ok(None);
            }
            b'$' => return Ok(self.dollar(i)),
            b'-' if self.bytes.get(*i + 1) == Some(&b'-') => {
                *i += 1;
                return Ok(Some(State::LineComment));
            }
            b'/' if self.bytes.get(*i + 1) == Some(&b'*') => {
                *i += 1;
                self.depth = 1;
                return Ok(Some(State::BlockComment));
            }
            _ => {}
        }

        if is_space(c) {
            self.last = *i + 1;
            return Ok(None);
        }

        if let Some(len) = special_operator(self.bytes, *i) {
            // `::=` is an operator run and not `::` followed by `=`. The three character check
            // above is why `->` survives the rule that a `-` never joins anything.
            if self.bytes.get(*i + len).is_some_and(|&next| is_operator_char_in_run(next)) {
                self.last = *i;
                return Ok(Some(State::Operator));
            }
            self.push(*i, *i + len, Kind::Operator);
            *i += len - 1;
            self.last = *i + 1;
            return Ok(None);
        }

        if is_single_byte_operator(c) {
            self.push(*i, *i + 1, Kind::Operator);
            self.last = *i + 1;
            return Ok(None);
        }

        if is_initial_number(c) {
            self.last = *i;
            return Ok(Some(State::Numeric));
        }

        // `E`, `X`, `B` and `N`, in either case, and only when the quote is the very next byte.
        // `SELECT e 'a'` is an identifier and then a string. Only `E` changes how the body is
        // read, and it is the only one of the four this tokenizer does anything else with.
        if is_string_prefix(c) && self.bytes.get(*i + 1) == Some(&b'\'') {
            self.last = *i;
            self.escape_string = c == b'E' || c == b'e';
            *i += 1;
            return Ok(Some(State::StringLiteral));
        }

        if is_operator_char(c) {
            self.last = *i;
            return Ok(Some(State::Operator));
        }

        self.last = *i;
        Ok(Some(State::Word))
    }

    /// `$` is three things and which one it is depends on what follows.
    fn dollar(&mut self, i: &mut usize) -> Option<State> {
        let Some(&next) = self.bytes.get(*i + 1) else {
            // Nothing after it, and upstream leaves `last` where it is, so this byte ends up in
            // whatever the final token turns out to be.
            return None;
        };
        if next.is_ascii_digit() {
            // `$1` is a parameter, and it is two tokens rather than one. The grammar spells it,
            // which is why the tokenizer does not have to.
            self.push(*i, *i + 1, Kind::Operator);
            return None;
        }

        // A tag runs to the next `$` and may contain only tag characters. Anything else and this
        // was `$name`, which is also two tokens.
        let mut close = None;
        for at in *i + 1..self.bytes.len() {
            if self.bytes[at] == b'$' {
                close = Some(at);
                break;
            }
            if !is_dollar_tag_char(self.bytes[at]) {
                break;
            }
        }
        let Some(close) = close else {
            self.push(*i, *i + 1, Kind::Operator);
            return None;
        };

        self.last = *i;
        self.dollar_tag = Span::new(*i as u32 + 1, close as u32);
        *i = close;
        Some(State::DollarQuoted)
    }

    fn numeric(&mut self, state: &mut State, i: &mut usize, c: u8) {
        if is_initial_number(c) {
            return;
        }
        // Only between two digits, which is why `1_000` is a thousand and `SELECT 1_` is `1`
        // aliased `_`.
        if c == b'_' && self.bytes.get(*i + 1).is_some_and(|&n| is_initial_number(n)) {
            return;
        }
        if is_scientific(c) && !is_scientific(self.bytes[*i - 1]) {
            // A digit has to be in there somewhere, which rules out `.e100` while allowing both
            // `1e5` and `.1e5`.
            if self.bytes[self.last].is_ascii_digit() || self.bytes[*i - 1].is_ascii_digit() {
                return;
            }
        }
        if (c == b'+' || c == b'-') && is_scientific(self.bytes[*i - 1]) {
            return;
        }

        // Give back anything on the end that is not a digit or a dot, which is what turns the `e`
        // of `1e+` back into something the next pass has to deal with.
        while !is_initial_number(self.bytes[*i - 1]) {
            *i -= 1;
        }
        self.push(self.last, *i, Kind::Number);
        *state = State::Standard;
        self.last = *i;
        *i -= 1;
    }

    fn operator(&mut self, state: &mut State, i: &mut usize, c: u8) {
        if c == b'/' && self.bytes.get(*i + 1) == Some(&b'*') {
            self.push_operator(self.last, *i);
            *state = State::Standard;
            self.last = *i;
            *i -= 1;
            return;
        }
        if !is_operator_char_in_run(c) {
            self.push_operator(self.last, *i);
            *state = State::Standard;
            self.last = *i;
            *i -= 1;
        }
    }

    fn word(&mut self, state: &mut State, i: &mut usize, c: u8) {
        // `$` is a legal non-initial identifier character, which is PostgreSQL's rule and is why
        // this one test is not part of `is_word_char`.
        if c == b'$' || is_word_char(c) {
            return;
        }
        self.push_word(self.last, *i);
        *state = State::Standard;
        self.last = *i;
        *i -= 1;
    }

    fn string_literal(&mut self, state: &mut State, i: &mut usize, c: u8) {
        if self.escape_string && c == b'\\' && *i + 1 < self.bytes.len() {
            *i += 1;
            return;
        }
        if c != b'\'' {
            return;
        }
        if self.bytes.get(*i + 1) == Some(&b'\'') {
            *i += 1;
            return;
        }
        self.push(self.last, *i + 1, Kind::String);
        self.last = *i + 1;
        self.escape_string = false;
        *state = State::Standard;
    }

    fn quoted_identifier(&mut self, state: &mut State, i: &mut usize, c: u8) -> Result<()> {
        if c != b'"' {
            return Ok(());
        }
        if self.bytes.get(*i + 1) == Some(&b'"') {
            *i += 1;
            return Ok(());
        }
        if *i + 1 == self.last + 2 {
            return Err(self.error("zero-length delimited identifier", self.last));
        }
        self.push(self.last, *i + 1, Kind::QuotedIdentifier);
        self.last = *i + 1;
        *state = State::Standard;
        Ok(())
    }

    fn block_comment(&mut self, state: &mut State, i: &mut usize, c: u8) {
        // Nested, which is the difference between commenting out a block that contains a comment
        // and getting a syntax error halfway down the file.
        if c == b'/' && self.bytes.get(*i + 1) == Some(&b'*') {
            *i += 1;
            self.depth += 1;
        } else if c == b'*' && self.bytes.get(*i + 1) == Some(&b'/') {
            *i += 1;
            self.depth -= 1;
            if self.depth == 0 {
                self.comment(self.last, *i + 1);
                self.last = *i + 1;
                *state = State::Standard;
            }
        }
    }

    fn dollar_quoted(&mut self, state: &mut State, i: &mut usize) {
        if self.bytes[*i] != b'$' || *i + 1 >= self.bytes.len() {
            return;
        }
        let start = *i + 1;
        let mut end = start;
        while end < self.bytes.len() && self.bytes[end] != b'$' {
            end += 1;
        }
        if end >= self.bytes.len() {
            return;
        }
        let tag = &self.bytes[self.dollar_tag.start as usize..self.dollar_tag.end as usize];
        if end - start != tag.len() || &self.bytes[start..end] != tag {
            return;
        }
        self.push(self.last, end + 1, Kind::String);
        *state = State::Standard;
        *i = end;
        self.last = *i + 1;
    }

    /// Push a bare word, having decided whether it is a keyword.
    ///
    /// A word is a keyword when it is in at least one class, which is not the same as being in
    /// the table. The 15 soft words are in the table with a mask of zero and are identifiers here,
    /// exactly as `PEGKeywordHelper::IsKeyword` has it, because their lists are the five class
    /// lists and a soft word is in none of them. They still keep their index, so `ORDER BY x
    /// ASCENDING` can match the literal without `SELECT ascending FROM t` becoming a syntax error.
    /// `spec/20-the-grammar.md` section 5.
    fn push_word(&mut self, start: usize, end: usize) {
        if start >= end {
            return;
        }
        let keyword = lookup(&self.query[start..end]);
        let kind = if classes(keyword) == 0 { Kind::Identifier } else { Kind::Keyword };
        let flags = self.gap_flags(start);
        self.tokens.push(Token { kind, flags, keyword, start: start as u32, end: end as u32 });
    }

    /// An operator run, minus a trailing `+` where PostgreSQL says to give it back.
    ///
    /// `SELECT 1 =+ 1` is `1 = +1`, and `SELECT 1 !=+ 1` goes looking for an operator named `!=+`,
    /// because the run in the second case contains a character from the special set. The rule is
    /// PostgreSQL's and it exists so that a user defined operator can be told apart from an
    /// operator followed by a signed number.
    fn push_operator(&mut self, start: usize, end: usize) {
        let special = self.bytes[start..end].iter().any(|&b| {
            matches!(b, b'~' | b'!' | b'@' | b'#' | b'%' | b'^' | b'&' | b'|' | b'`' | b'?')
        });
        let mut cut = end;
        if !special {
            while cut > start && self.bytes[cut - 1] == b'+' {
                cut -= 1;
            }
        }
        self.push(start, cut, Kind::Operator);
        for at in cut..end {
            self.push(at, at + 1, Kind::Operator);
        }
    }

    /// Record a comment. Nothing is pushed, because a comment is not a token anywhere the parser
    /// can see, but where the block ones were has to be remembered so the next token can say it
    /// was preceded by one.
    fn comment(&mut self, start: usize, end: usize) {
        if end >= start + 2 && &self.bytes[start..start + 2] == b"/*" {
            self.block_comment_at = Some(start);
        }
    }

    /// Push a token that is not a bare word, unless it is empty.
    ///
    /// The empty check is upstream's and it is what lets several states push unconditionally at a
    /// boundary without first asking whether they have anything.
    ///
    /// One divergence, and it is in a field rather than in a token. Upstream reaches around
    /// `PushToken` for single byte operators, special operators, a trimmed `+`, a `$` and a `;`,
    /// so those five arrive with both gap flags clear no matter what was in the gap. We set them
    /// on everything. Nothing reads a gap flag on an operator today, and a rule that holds for
    /// every token is one fewer thing to remember when something starts to.
    fn push(&mut self, start: usize, end: usize, kind: Kind) {
        if start >= end {
            return;
        }
        let flags = self.gap_flags(start);
        self.tokens.push(Token {
            kind,
            flags,
            keyword: NOT_A_KEYWORD,
            start: start as u32,
            end: end as u32,
        });
    }

    fn push_flagged(&mut self, start: usize, end: usize, kind: Kind, extra: Flags) {
        self.push(start, end, kind);
        if let Some(token) = self.tokens.last_mut() {
            token.flags = token.flags.with(extra);
        }
    }

    /// What was in the gap between the previous token and `start`.
    ///
    /// Two literals separated by whitespace containing a newline are one literal, a line comment
    /// between them keeps the join, and a block comment breaks it. That rule is PostgreSQL's,
    /// DuckDB kept it, and it is the only reason either flag exists.
    fn gap_flags(&self, start: usize) -> Flags {
        let Some(previous) = self.tokens.last() else { return Flags::default() };
        let from = previous.end as usize;
        let mut flags = Flags::default();
        if self.block_comment_at.is_some_and(|at| at >= from && at < start) {
            flags = flags.with(Flags::BLOCK_COMMENT);
        }
        if self.bytes[from..start.min(self.bytes.len())].iter().any(|&b| b == b'\n' || b == b'\r') {
            flags = flags.with(Flags::NEWLINE);
        }
        flags
    }

    fn error(&self, message: impl Into<String>, at: usize) -> Error {
        Error::parser(message).with_span(Span::new(at as u32, self.bytes.len() as u32))
    }
}

/// The index of `word` in the generated keyword table, or [`NOT_A_KEYWORD`].
///
/// ASCII folded into a fixed buffer, because every keyword is a plain ASCII word and the longest
/// is 15 bytes, so a longer candidate cannot be one and never touches the table.
pub fn lookup(word: &str) -> u16 {
    if word.len() > LONGEST {
        return NOT_A_KEYWORD;
    }
    let mut folded = [0u8; LONGEST];
    for (slot, byte) in folded.iter_mut().zip(word.bytes()) {
        *slot = byte.to_ascii_lowercase();
    }
    let folded = &folded[..word.len()];
    // The table is sorted, so this is one binary search over 514 entries, which is nine
    // comparisons and fits in a handful of cache lines.
    match KEYWORDS.binary_search_by(|(candidate, _)| candidate.as_bytes().cmp(folded)) {
        Ok(at) => at as u16,
        Err(_) => NOT_A_KEYWORD,
    }
}

/// The classes `word` belongs to, or zero.
///
/// Zero for a word that is in the table with no class, which is one of the 15 soft words, and
/// zero for a word that is not in the table at all. Those two are the same answer to the only
/// question this function is asked, which is whether the word blocks an identifier here.
pub fn classes(keyword: u16) -> u8 {
    if keyword == NOT_A_KEYWORD { 0 } else { KEYWORDS[keyword as usize].1 }
}

const fn is_space(c: u8) -> bool {
    matches!(c, b' ' | b'\t' | b'\n' | 0x0b | 0x0c | b'\r')
}

/// The dozen characters that are always their own token and never join a run.
///
/// `-` and `#` being in here is the surprise. It is why `SELECT 1 =- 1` is `1 = -1` and why `--`
/// can be a comment without ever having to be told it is not an operator.
const fn is_single_byte_operator(c: u8) -> bool {
    matches!(c, b'(' | b')' | b'{' | b'}' | b'[' | b']' | b',' | b'?' | b'$' | b'-' | b'#')
}

/// PostgreSQL's operator character set, which is every ASCII punctuation character except `_`.
const fn is_operator_char(c: u8) -> bool {
    if c == b'_' {
        return false;
    }
    matches!(c, b'!'..=b'/' | b':'..=b'@' | b'['..=b'`' | b'{'..=b'~')
}

/// Whether `c` can continue an operator run.
///
/// The single byte operators are excluded, and so are the five characters that always end one:
/// `'`, `-`, `;`, `"` and `.`. `-` is in both lists and that is not redundant, since the first
/// list is also consulted where the second is not.
const fn is_operator_char_in_run(c: u8) -> bool {
    if is_single_byte_operator(c) || is_control_flow(c) {
        return false;
    }
    is_operator_char(c)
}

const fn is_control_flow(c: u8) -> bool {
    matches!(c, b'\'' | b'-' | b';' | b'"' | b'.')
}

/// Whether `c` can continue a bare word.
///
/// Anything that is not punctuation, whitespace or a delimiter, which by the arithmetic above
/// includes every byte at or above 0x80. So an identifier may be any UTF-8 the user likes, and
/// the tokenizer never has to decode it to find that out.
const fn is_word_char(c: u8) -> bool {
    if is_single_byte_operator(c) || is_operator_char(c) || is_space(c) || is_control_flow(c) {
        return false;
    }
    true
}

/// A digit or a dot, which is both what starts a number and what can appear anywhere in one.
///
/// A dot being unconditional here is why `SELECT 1.2.3` is a single number token rather than
/// three things. What it means is somebody else's problem, which is exactly how upstream has it.
const fn is_initial_number(c: u8) -> bool {
    c.is_ascii_digit() || c == b'.'
}

const fn is_scientific(c: u8) -> bool {
    c == b'e' || c == b'E'
}

const fn is_string_prefix(c: u8) -> bool {
    matches!(c, b'N' | b'n' | b'X' | b'x' | b'E' | b'e' | b'B' | b'b')
}

/// A-Z, a-z, 0-9, `_`, and anything at or above 0x80. Digits are only legal after the first byte
/// and the caller checks that.
const fn is_dollar_tag_char(c: u8) -> bool {
    c.is_ascii_alphanumeric() || c == b'_' || c >= 0x80
}

/// The six sequences checked before the maximal run, longest first.
///
/// `->` is here because `-` is a single byte operator and would otherwise never join anything,
/// which would leave the JSON arrow unspellable.
fn special_operator(bytes: &[u8], at: usize) -> Option<usize> {
    if bytes[at..].starts_with(b"->>") {
        return Some(3);
    }
    for candidate in [b"::".as_slice(), b":=", b"->", b"**", b"//"] {
        if bytes[at..].starts_with(candidate) {
            return Some(2);
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::{classes, lookup, tokenize};
    use crate::generated::keywords::{RESERVED, UNRESERVED};
    use crate::token::{Flags, Kind, NOT_A_KEYWORD, Token};

    /// Every token but the sentinel, as the pair worth asserting on.
    fn scan(query: &str) -> Vec<(Kind, &str)> {
        let tokens = tokenize(query).expect("tokenizes");
        assert_eq!(tokens.last().map(|t| t.kind), Some(Kind::EndOfInput));
        assert_eq!(tokens.iter().filter(|t| t.kind == Kind::EndOfInput).count(), 1);
        tokens[..tokens.len() - 1].iter().map(|t| (t.kind, t.text(query))).collect()
    }

    fn texts(query: &str) -> Vec<&str> {
        scan(query).into_iter().map(|(_, text)| text).collect()
    }

    fn all(query: &str) -> Vec<Token> {
        tokenize(query).expect("tokenizes")
    }

    fn message(query: &str) -> String {
        tokenize(query).expect_err("fails").message().to_string()
    }

    #[test]
    fn the_empty_query_is_one_sentinel() {
        let tokens = tokenize("").expect("tokenizes");
        assert_eq!(tokens.len(), 1);
        assert_eq!(tokens[0].kind, Kind::EndOfInput);
        assert_eq!(tokens[0].span(), rudb_common::Span::new(0, 0));
        assert!(scan("   \t\n  ").is_empty());
    }

    #[test]
    fn a_word_in_a_class_is_a_keyword_and_one_in_none_is_not() {
        assert_eq!(scan("SELECT"), [(Kind::Keyword, "SELECT")]);
        assert_eq!(scan("banana"), [(Kind::Identifier, "banana")]);
    }

    #[test]
    fn case_is_matched_but_not_folded() {
        // DuckDB is case insensitive and case preserving, and unlike PostgreSQL that holds for
        // quoted identifiers too. So the keyword is recognized whatever its case and the bytes
        // come back exactly as written. Folding here would be the wrong answer twice over.
        for spelling in ["select", "SELECT", "SeLeCt"] {
            let tokens = all(spelling);
            assert_eq!(tokens[0].kind, Kind::Keyword);
            assert_eq!(tokens[0].text(spelling), spelling);
            assert_eq!(tokens[0].keyword, lookup("select"));
        }
        assert_eq!(scan(r#""Foo""#), [(Kind::QuotedIdentifier, r#""Foo""#)]);
    }

    #[test]
    fn a_soft_word_keeps_its_index_and_stays_an_identifier() {
        // ASCENDING is spelled by a rule and is in none of the five lists. If it came back as a
        // keyword then `SELECT ascending FROM t` would stop working, and if it came back without
        // an index then `ORDER BY x ASCENDING` could not be filtered on the literal.
        let tokens = all("ascending");
        assert_eq!(tokens[0].kind, Kind::Identifier);
        assert_ne!(tokens[0].keyword, NOT_A_KEYWORD);
        assert_eq!(classes(tokens[0].keyword), 0);

        let tokens = all("banana");
        assert_eq!(tokens[0].keyword, NOT_A_KEYWORD);
        assert_eq!(classes(tokens[0].keyword), 0);
    }

    #[test]
    fn the_classes_come_back_off_the_index() {
        assert_eq!(classes(lookup("select")) & RESERVED, RESERVED);
        assert_eq!(classes(lookup("abort")) & UNRESERVED, UNRESERVED);
        assert_eq!(lookup("supercalifragilistic"), NOT_A_KEYWORD);
        assert_eq!(lookup(""), NOT_A_KEYWORD);
    }

    #[test]
    fn a_quoted_identifier_is_never_a_keyword() {
        let tokens = all(r#""select""#);
        assert_eq!(tokens[0].kind, Kind::QuotedIdentifier);
        assert_eq!(tokens[0].keyword, NOT_A_KEYWORD);
        assert_eq!(scan(r#""a""b""#), [(Kind::QuotedIdentifier, r#""a""b""#)]);
    }

    #[test]
    fn a_dollar_is_an_identifier_character_after_the_first_byte() {
        assert_eq!(scan("a$b"), [(Kind::Identifier, "a$b")]);
    }

    #[test]
    fn any_byte_above_ascii_is_an_identifier_character() {
        // Nothing decodes UTF-8 here. The arithmetic in `is_word_char` says every byte at or
        // above 0x80 continues a word, which is how an identifier in any script gets through
        // without the tokenizer knowing what script it is.
        assert_eq!(scan("SELECT café"), [(Kind::Keyword, "SELECT"), (Kind::Identifier, "café")]);
    }

    #[test]
    fn a_number_swallows_more_than_a_number() {
        // Every one of these is a single NUMBER token upstream and the parser deals with what it
        // means. `1.2.3` in particular is not three things.
        assert_eq!(scan("1.2.3"), [(Kind::Number, "1.2.3")]);
        assert_eq!(scan("1_000"), [(Kind::Number, "1_000")]);
        assert_eq!(scan("1e5"), [(Kind::Number, "1e5")]);
        assert_eq!(scan(".1e5"), [(Kind::Number, ".1e5")]);
        assert_eq!(scan("1e-5"), [(Kind::Number, "1e-5")]);
        assert_eq!(scan("1.e5"), [(Kind::Number, "1.e5")]);
    }

    #[test]
    fn a_trailing_e_stays_on_the_number() {
        // `SELECT 1e` is one NUMBER token "1e" and not `1` aliased `e`, because the tokenizer
        // takes the `e` and only the parser is in a position to object.
        assert_eq!(scan("SELECT 1e"), [(Kind::Keyword, "SELECT"), (Kind::Number, "1e")]);
        assert_eq!(scan("SELECT 1e+"), [(Kind::Keyword, "SELECT"), (Kind::Number, "1e+")]);
    }

    #[test]
    fn what_the_number_cannot_use_it_gives_back() {
        // The backtrack at the end of NUMERIC only keeps digits and dots, so `1e+ 1` unwinds the
        // whole exponent it started and the `e` comes back as a name.
        assert_eq!(
            scan("SELECT 1e+ 1"),
            [
                (Kind::Keyword, "SELECT"),
                (Kind::Number, "1"),
                (Kind::Identifier, "e"),
                (Kind::Operator, "+"),
                (Kind::Number, "1"),
            ]
        );
        assert_eq!(scan("1_"), [(Kind::Number, "1"), (Kind::Identifier, "_")]);
        assert_eq!(scan("1__0"), [(Kind::Number, "1"), (Kind::Identifier, "__0")]);
        // There is no hex literal. `0x1F` is zero and then a name, which is worth knowing before
        // somebody reports it as a bug in the parser.
        assert_eq!(scan("0x1F"), [(Kind::Number, "0"), (Kind::Identifier, "x1F")]);
        assert_eq!(scan(".e100"), [(Kind::Number, "."), (Kind::Identifier, "e100")]);
    }

    #[test]
    fn a_minus_never_joins_an_operator_run() {
        // `-` is a single byte operator, which is what makes `SELECT 1 =- 1` a subtraction of a
        // negative one rather than a call to an operator named `=-`.
        assert_eq!(texts("1-1"), ["1", "-", "1"]);
        assert_eq!(texts("SELECT 1 =- 1"), ["SELECT", "1", "=", "-", "1"]);
        assert_eq!(texts("(a,b)"), ["(", "a", ",", "b", ")"]);
    }

    #[test]
    fn the_postgres_plus_rule_decides_where_a_run_ends() {
        // An operator cannot end in `+` unless it contains one of ~ ! @ # % ^ & | ` ?. This is
        // the rule that lets a user defined operator be told apart from an operator and a sign.
        assert_eq!(texts("SELECT 1 =+ 1"), ["SELECT", "1", "=", "+", "1"]);
        assert_eq!(texts("SELECT 1 !=+ 1"), ["SELECT", "1", "!=+", "1"]);
        assert_eq!(texts("SELECT 1 =++ 1"), ["SELECT", "1", "=", "+", "+", "1"]);
        assert_eq!(texts("SELECT 1 ++ 1"), ["SELECT", "1", "+", "+", "1"]);
    }

    #[test]
    fn the_special_operators_are_checked_before_the_run() {
        assert_eq!(texts("a->>'b'"), ["a", "->>", "'b'"]);
        assert_eq!(texts("a->'b'"), ["a", "->", "'b'"]);
        assert_eq!(texts("a::b"), ["a", "::", "b"]);
        assert_eq!(texts("a//b"), ["a", "//", "b"]);
        assert_eq!(texts("2**3"), ["2", "**", "3"]);
        // But only when what follows is not itself an operator character, in which case the
        // maximal run wins and it is one token.
        assert_eq!(texts("a::=b"), ["a", "::=", "b"]);
    }

    #[test]
    fn a_block_comment_can_end_an_operator_run() {
        assert_eq!(texts("1+/*c*/2"), ["1", "+", "2"]);
    }

    #[test]
    fn a_comment_is_not_a_token_but_a_block_one_leaves_a_mark() {
        assert_eq!(texts("SELECT --x\n1"), ["SELECT", "1"]);
        assert_eq!(texts("SELECT /*x*/ 1"), ["SELECT", "1"]);
        assert_eq!(texts("SELECT --x"), ["SELECT"]);

        let tokens = all("SELECT /*x*/ 1");
        assert!(tokens[1].flags.has(Flags::BLOCK_COMMENT));
        assert!(!tokens[1].flags.has(Flags::NEWLINE));

        // A line comment is not a block comment, and the newline that ends it still counts.
        let tokens = all("SELECT --x\n1");
        assert!(!tokens[1].flags.has(Flags::BLOCK_COMMENT));
        assert!(tokens[1].flags.has(Flags::NEWLINE));
    }

    #[test]
    fn the_first_token_is_preceded_by_nothing() {
        let tokens = all("\n/*x*/ SELECT");
        assert_eq!(tokens[0].flags, Flags::default());
    }

    #[test]
    fn block_comments_nest() {
        // The reason this matters is commenting out a block that already contains a comment. In
        // PostgreSQL it works, in most SQL dialects it does not, and DuckDB followed PostgreSQL.
        assert_eq!(texts("SELECT /* a /* b */ c */ 1"), ["SELECT", "1"]);
        assert_eq!(
            message("SELECT /* a /* b */ 1"),
            "unterminated /* comment at or near \"/* a /* b */ 1\""
        );
    }

    #[test]
    fn a_string_keeps_its_quotes_and_its_escapes() {
        assert_eq!(scan("'it''s'"), [(Kind::String, "'it''s'")]);
        assert_eq!(scan("''"), [(Kind::String, "''")]);
        assert_eq!(texts("'a' 'b'"), ["'a'", "'b'"]);
    }

    #[test]
    fn only_the_e_prefix_changes_how_a_string_is_read() {
        // In an E string a backslash escapes the next byte, so the doubled quote at the end is
        // one escaped quote and then the close. Without the prefix the backslash is an ordinary
        // character, the doubled quote is an escape, and the literal runs off the end.
        assert_eq!(scan(r"E'\''"), [(Kind::String, r"E'\''")]);
        assert_eq!(message(r"'\''"), "unterminated string literal");
        for prefix in ["X", "x", "B", "b", "N", "n", "E", "e"] {
            let query = format!("{prefix}'a'");
            assert_eq!(tokenize(&query).expect("tokenizes")[0].kind, Kind::String);
        }
        // The quote has to be the very next byte, otherwise it is a name and then a string.
        assert_eq!(texts("x 'a'"), ["x", "'a'"]);
    }

    #[test]
    fn a_dollar_quoted_string_is_one_token_and_its_tag_has_to_match() {
        assert_eq!(scan("$$abc$$"), [(Kind::String, "$$abc$$")]);
        assert_eq!(scan("$tag$abc$tag$"), [(Kind::String, "$tag$abc$tag$")]);
        assert_eq!(scan("$tag$a$other$b$tag$"), [(Kind::String, "$tag$a$other$b$tag$")]);
        assert_eq!(scan("$$it's fine$$"), [(Kind::String, "$$it's fine$$")]);
    }

    #[test]
    fn an_unterminated_dollar_quote_is_a_token_and_not_an_error() {
        // The other three unterminated forms throw. This one does not, which is upstream's
        // choice and not ours, and the flag is how the difference reaches the matcher.
        let tokens = all("$$abc");
        assert_eq!(tokens[0].kind, Kind::String);
        assert!(tokens[0].flags.has(Flags::UNTERMINATED));
        assert_eq!(tokens[0].text("$$abc"), "$$abc");
    }

    #[test]
    fn a_parameter_is_two_tokens() {
        // There is no parameter token kind. The grammar spells `$` followed by a number or a
        // name, so the tokenizer never has to decide which one it is looking at.
        assert_eq!(scan("$1"), [(Kind::Operator, "$"), (Kind::Number, "1")]);
        assert_eq!(scan("$banana"), [(Kind::Operator, "$"), (Kind::Identifier, "banana")]);
        assert_eq!(scan("?"), [(Kind::Operator, "?")]);
        // A lone dollar has nothing after it to decide with and falls out as an identifier.
        assert_eq!(scan("$"), [(Kind::Identifier, "$")]);
    }

    #[test]
    fn a_semicolon_is_its_own_kind() {
        assert_eq!(
            scan("SELECT 1; SELECT 2"),
            [
                (Kind::Keyword, "SELECT"),
                (Kind::Number, "1"),
                (Kind::Terminator, ";"),
                (Kind::Keyword, "SELECT"),
                (Kind::Number, "2"),
            ]
        );
        assert_eq!(scan(";"), [(Kind::Terminator, ";")]);
    }

    #[test]
    fn the_four_errors_are_the_four_upstream_throws() {
        assert_eq!(message("SELECT /* x"), "unterminated /* comment at or near \"/* x\"");
        assert_eq!(message("SELECT 'x"), "unterminated string literal");
        assert_eq!(message("SELECT \"x"), "unterminated quoted identifier");
        assert_eq!(message("SELECT \"\""), "zero-length delimited identifier");
        // And an error points at where the trouble started, not at the end of the query.
        assert_eq!(tokenize("SELECT 'x").expect_err("fails").span().map(|s| s.start), Some(7));
    }

    #[test]
    fn every_span_lands_where_the_text_is() {
        let query = "SELECT a, /*c*/ 'b' || $$d$$ FROM t;";
        for token in tokenize(query).expect("tokenizes") {
            assert!(token.end as usize <= query.len());
            assert!(token.start <= token.end);
            if token.kind != Kind::EndOfInput {
                assert!(!token.text(query).is_empty());
            }
        }
    }

    #[test]
    fn a_real_query_comes_out_the_way_it_reads() {
        assert_eq!(
            scan("SELECT count(*) FROM t WHERE x > 5 AND y::VARCHAR = 'a';"),
            [
                (Kind::Keyword, "SELECT"),
                (Kind::Identifier, "count"),
                (Kind::Operator, "("),
                (Kind::Operator, "*"),
                (Kind::Operator, ")"),
                (Kind::Keyword, "FROM"),
                (Kind::Identifier, "t"),
                (Kind::Keyword, "WHERE"),
                (Kind::Identifier, "x"),
                (Kind::Operator, ">"),
                (Kind::Number, "5"),
                (Kind::Keyword, "AND"),
                (Kind::Identifier, "y"),
                (Kind::Operator, "::"),
                (Kind::Keyword, "VARCHAR"),
                (Kind::Operator, "="),
                (Kind::String, "'a'"),
                (Kind::Terminator, ";"),
            ]
        );
    }
}
