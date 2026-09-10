//! What the tokenizer produces and what the matcher consumes.
//!
//! A token is twelve bytes and holds no string. The text stays in the query, the token holds a
//! span into it, and nothing is decoded here: a string keeps its quotes and its escapes, a number
//! keeps its underscores, an identifier keeps its case. Decoding is the transformer's job, and
//! leaving it there is what keeps the whole token vector in cache for a query of any sane size.
//!
//! `spec/20-the-grammar.md` section 7 is the behaviour this has to match and why matching it is
//! the largest single compatibility risk in the front end.

use rudb_common::Span;

/// A word is not a keyword.
///
/// `u16::MAX` rather than an `Option<u16>`, which would be four bytes and would put a branch on
/// the path that reads the keyword out. There are 514 keywords and there is no prospect of 65,535.
pub const NOT_A_KEYWORD: u16 = u16::MAX;

/// What a token is.
///
/// DuckDB has one `IDENTIFIER` type covering both a bare word and a quoted one, and recovers the
/// difference by looking at the first byte where it matters. We split them, because the grammar
/// has a `QuotedIdentifier` rule and asking the token is cheaper and harder to get wrong than
/// asking the source text. The two are otherwise treated alike everywhere upstream treats them
/// alike, which is nearly everywhere.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[repr(u8)]
pub enum Kind {
    /// A bare word that is in no keyword class. `foo`, and also `ascending`, which is spelled by
    /// a rule and is in no class, so it is a perfectly good column name.
    Identifier,
    /// A word in double quotes. Still an identifier, never a keyword, and case preserving in
    /// exactly the same way a bare one is.
    QuotedIdentifier,
    /// A bare word that is in at least one keyword class. `keyword` on this token says which.
    Keyword,
    /// A numeric literal, with its underscores, its exponent and its decimal point as written.
    Number,
    /// A string literal, with its quotes. Single quoted, dollar quoted, or one of the four
    /// prefixed forms, all of which arrive here as one token.
    String,
    /// Punctuation or an operator run. `(`, `,`, `::`, `!~~*` and `+` are all this.
    Operator,
    /// A `;`. Its own kind because `Program <- TopLevelStatement*` consumes it as one, and
    /// because a statement boundary that the tokenizer decided would be a statement boundary the
    /// grammar cannot change.
    Terminator,
    /// The sentinel at the end. Always present, always last, always exactly one.
    EndOfInput,
}

impl Kind {
    /// Whether this is a name, either spelling.
    pub const fn is_identifier(self) -> bool {
        matches!(self, Kind::Identifier | Kind::QuotedIdentifier)
    }
}

/// Facts about the gap before a token, which the grammar cannot see and two rules need.
///
/// A bitfield rather than three `bool`s so that a token stays twelve bytes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Flags(u8);

impl Flags {
    /// There was a line break between the end of the previous token and the start of this one.
    pub const NEWLINE: Flags = Flags(1 << 0);
    /// A block comment ended in the gap before this token.
    pub const BLOCK_COMMENT: Flags = Flags(1 << 1);
    /// The token ran to the end of the input without its closing delimiter. Only a dollar quoted
    /// string reaches the matcher this way; the other unterminated forms are errors.
    pub const UNTERMINATED: Flags = Flags(1 << 2);

    /// Whether every flag in `other` is set here.
    pub const fn has(self, other: Flags) -> bool {
        self.0 & other.0 == other.0
    }

    /// This set with `other` added.
    pub const fn with(self, other: Flags) -> Flags {
        Flags(self.0 | other.0)
    }
}

/// One token. Twelve bytes, no allocation, no owned text.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Token {
    /// What it is.
    pub kind: Kind,
    /// What was in the gap before it.
    pub flags: Flags,
    /// The index into `generated::keywords::KEYWORDS`, or `NOT_A_KEYWORD`.
    ///
    /// Resolved once here rather than at every position the matcher considers the token, because
    /// the matcher considers most tokens many times and the fold plus binary search is the
    /// expensive part of asking.
    pub keyword: u16,
    /// Where it starts, as a byte offset into the query.
    pub start: u32,
    /// One past where it ends.
    pub end: u32,
}

impl Token {
    /// The span this token covers.
    pub const fn span(self) -> Span {
        Span::new(self.start, self.end)
    }

    /// The text of this token, given the query it came from.
    ///
    /// Both bounds came from a scan over the same string and always land on a character boundary,
    /// so this cannot panic on well formed input. It is written as a slice rather than a checked
    /// `get` because a token whose bounds are not on a boundary is a bug in the tokenizer and
    /// silently returning nothing would hide it.
    pub fn text(self, query: &str) -> &str {
        &query[self.start as usize..self.end as usize]
    }
}

#[cfg(test)]
mod tests {
    use super::{Flags, Kind, Token};

    #[test]
    fn a_token_is_twelve_bytes() {
        // Not a style preference. A hundred token query is one cache line per ten tokens, and the
        // matcher walks the vector many times over. If this ever fails, something grew a field
        // that should have been computed instead.
        assert_eq!(size_of::<Token>(), 12);
    }

    #[test]
    fn the_flags_combine_and_read_back() {
        let flags = Flags::default().with(Flags::NEWLINE).with(Flags::BLOCK_COMMENT);
        assert!(flags.has(Flags::NEWLINE));
        assert!(flags.has(Flags::BLOCK_COMMENT));
        assert!(!flags.has(Flags::UNTERMINATED));
        assert!(!Flags::default().has(Flags::NEWLINE));
    }

    #[test]
    fn both_spellings_of_a_name_are_identifiers() {
        assert!(Kind::Identifier.is_identifier());
        assert!(Kind::QuotedIdentifier.is_identifier());
        assert!(!Kind::Keyword.is_identifier());
        assert!(!Kind::String.is_identifier());
    }
}
