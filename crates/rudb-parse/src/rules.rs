//! The shape of the generated rule table, and the filter that reads it.
//!
//! `generated::rules` is data. This is the handful of types that give it meaning, and they are
//! written by hand because they are an interface: the generator in `xtask` writes discriminants
//! that have to mean the same thing here, and there is a test on each side that says so.
//!
//! The one idea worth stating on its own is the FIRST filter. A choice in this grammar can have
//! forty alternatives, `Statement` has thirty six, and upstream tries them in order, descending
//! into each one far enough to fail. Every node here carries a 64 bit set of the token keys it can
//! begin with, and a token maps to exactly one of those keys, so an alternative that cannot
//! possibly match is skipped on one AND rather than on a subtree walk. The set is a superset by
//! construction: keywords share 58 buckets, so a bit that is set may still fail, and a bit that is
//! clear can never match. Being wrong in that direction costs a wasted attempt and never changes
//! what the parser accepts, which is what makes it safe to put in front of a dialect we are
//! copying rather than defining.
//!
//! `spec/20-the-grammar.md` sections 3 and 5.

use crate::token::{Kind, NOT_A_KEYWORD, Token};

/// What a node is.
///
/// The discriminants are written into `generated::rules` as `Op::Name`, so they are not load
/// bearing on their own, but `xtask`'s copy of this enum has to have the same variants in the same
/// order for the generator to be able to name them. `the_ops_match_the_generator` is that check.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum Op {
    /// A word. `a` indexes `generated::keywords::KEYWORDS`, and the comparison is an index compare
    /// because the tokenizer already resolved the token's word to the same table.
    Keyword = 0,
    /// Punctuation or an operator. `a` indexes `SYMBOLS` and the comparison is on text.
    Symbol = 1,
    /// A reference to a rule. `a` is the rule index.
    Rule = 2,
    /// `a` is a start index into `CHILDREN`, `b` is how many. All of them, in order.
    Sequence = 3,
    /// Same layout. Ordered choice, first success wins, no backtracking into a taken alternative.
    Choice = 4,
    /// `a` is the child node. Matches it or matches nothing.
    Optional = 5,
    /// `a` is the child node. One or more. `X*` is `Optional(Repeat(X))` in the table, because
    /// that is what upstream builds and a separate zero or more node would be a second thing to
    /// keep in step for no gain.
    Repeat = 6,
    /// An identifier matcher. `a` is a `Suggestion`, `flags` bit 0 is `RESERVED`.
    Identifier = 7,
    /// A numeric literal.
    Number = 8,
    /// A string literal, including its adjacent continuations.
    String = 9,
    /// An operator token, subject to the exclusions in `OperatorMatcher`.
    Operator = 10,
    /// The end of the input.
    EndOfInput = 11,
    /// A word in one of the five keyword classes. `a` is the class mask.
    ///
    /// The grammar spells these as an ordered choice of two hundred literals, because a PEG has no
    /// way to say "a word in this set". Upstream compiles that to two hundred `KeywordMatcher`
    /// objects and tries them in turn. The words in a list are distinct and a `KeywordMatcher` is a
    /// case insensitive text compare, so membership in the list is exactly a mask test on the class
    /// the tokenizer already resolved, and the two are the same predicate.
    KeywordClass = 12,
}

/// One node. Twenty four bytes, and everything the matcher needs to decide what to do with it.
///
/// The FIRST set and the nullable bit live in here rather than in two arrays beside it. They used
/// to be parallel tables, on the theory that the filter could read eight bytes of `FIRST` and skip
/// the node entirely, and that theory was wrong in the case that matters. A node that survives the
/// filter is loaded immediately afterwards, and surviving is the common case: the filter is there
/// to cut the thirty six alternatives of `Statement` down, and the one that matches still has to be
/// walked. So the old layout paid three cache lines on every node it did not reject and saved two
/// on every node it did, and the walk visits far more of the first kind.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Node {
    /// What this node can start with, as a set of token keys. A superset, always.
    pub first: u64,
    pub a: u32,
    pub b: u32,
    pub op: Op,
    /// Per op, plus `NULLABLE`, which every op can carry.
    pub flags: u8,
}

impl Node {
    /// On an `Identifier` node: the keyword check is dropped, so any word matches.
    ///
    /// This is the whole of `ReservedIdentifierMatcher`. It is worth knowing that upstream applies
    /// it to the rule named `ReservedKeyword`, so a grammar rule that reads
    /// `ColLabel <- ReservedKeyword / ...` does not test for a reserved word, it accepts any word
    /// at all. Reading the grammar text alone would get that backwards.
    pub const RESERVED: u8 = 1 << 0;

    /// This node can match without consuming a token, so its FIRST set says nothing about whether
    /// it applies and the filter has to let it through.
    pub const NULLABLE: u8 = 1 << 1;

    /// Whether a node could possibly begin with this token.
    ///
    /// False means it cannot, and that is the only answer the caller may act on. True means try it.
    /// A nullable node always answers true.
    pub fn can_start(self, key: u64) -> bool {
        self.flags & Self::NULLABLE != 0 || self.first & key != 0
    }

    /// The children of a sequence or a choice.
    pub fn children(self) -> &'static [u32] {
        &crate::generated::rules::CHILDREN[self.a as usize..(self.a + self.b) as usize]
    }
}

/// One rule.
#[derive(Debug, Clone, Copy)]
pub struct Rule {
    pub name: &'static str,
    /// The node its body compiled to. The matcher does not read this. A `Rule` node carries the
    /// same number in its `b`, so entering a rule is a field of a node already in a register rather
    /// than an index into a second table. This is here for the name lookup and for the tests.
    pub root: u32,
    /// Whether upstream memoizes it. Twenty two rules do, and they are the ones deep in the
    /// expression grammar that a failing alternative re-enters at the same position over and over.
    pub memoized: bool,
}

/// What an identifier matcher was built to suggest.
///
/// Kept rather than reduced to the two answers it implies, because upstream derives both from it
/// and keeping the derivation in one place is how the two stay comparable. `identifier_matcher.hpp`
/// is the source.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u32)]
pub enum Suggestion {
    Variable = 0,
    CatalogName = 1,
    SchemaName = 2,
    TableName = 3,
    ColumnName = 4,
    ScalarFunctionName = 5,
    TableFunctionName = 6,
    TypeName = 7,
    PragmaName = 8,
    SettingName = 9,
    FileName = 10,
}

impl Suggestion {
    /// Which keyword class may be used as a bare word here, on top of unreserved.
    ///
    /// Type name positions allow the type name class, both function name positions allow the
    /// combined type and function class, and everything else allows the column name class. That
    /// `TypeFuncKeyword <- TypeNameKeyword / FuncNameKeyword` is a rule in the grammar and also a
    /// category in the matcher is not a coincidence, it is the same union written once.
    /// `ParsedGrammarKeywordHelper`'s constructor holds a table of five rule names against five
    /// keyword sets, and the entry for `typefunc_keyword_map` names that rule, which it then walks
    /// through its references to collect the words. So the category is not a sixth list somebody
    /// has to keep in step with the other five, it is what that one line of the grammar says.
    pub const fn allowed_class(self) -> u8 {
        use crate::generated::keywords::{COLUMN_NAME, FUNC_NAME, TYPE_NAME};
        match self {
            Suggestion::TypeName => TYPE_NAME,
            Suggestion::ScalarFunctionName | Suggestion::TableFunctionName => TYPE_NAME | FUNC_NAME,
            _ => COLUMN_NAME,
        }
    }

    /// Whether a single quoted string is accepted where this name is expected.
    ///
    /// Two positions only. `FROM 'file.parquet'` is the reason, and `SELECT 'x' FROM t` staying a
    /// string literal rather than becoming a column reference is the reason it is only two.
    pub const fn supports_string_literal(self) -> bool {
        matches!(self, Suggestion::TableName | Suggestion::FileName)
    }
}

/// How many bits of a FIRST set go to token kinds before the keyword buckets start.
pub const KIND_BITS: u32 = 6;
/// The keyword buckets, being the rest of the 64.
pub const BUCKETS: u32 = 64 - KIND_BITS;

/// A bare or quoted name.
pub const FIRST_IDENT: u64 = 1 << 0;
/// A numeric literal.
pub const FIRST_NUMBER: u64 = 1 << 1;
/// A string literal.
pub const FIRST_STRING: u64 = 1 << 2;
/// An operator or a piece of punctuation.
pub const FIRST_OPERATOR: u64 = 1 << 3;
/// A `;`.
pub const FIRST_TERMINATOR: u64 = 1 << 4;
/// The end of the input.
pub const FIRST_END: u64 = 1 << 5;
/// Every keyword bucket at once, which is what an identifier matcher accepts, because which words
/// it takes depends on the class and on the position and the filter is not the place to decide it.
pub const FIRST_ANY_KEYWORD: u64 = !((1 << KIND_BITS) - 1);

/// Which bucket a keyword falls in.
pub const fn bucket(index: u32) -> u64 {
    1 << (KIND_BITS + index % BUCKETS)
}

/// The one FIRST bit this token sets.
///
/// Exactly one bit, so the filter is one AND against the node's set, with no loop and no branch.
pub fn token_key(token: Token) -> u64 {
    match token.kind {
        Kind::Identifier | Kind::QuotedIdentifier => FIRST_IDENT,
        // A word the tokenizer resolved to the table. A word in no class arrives as `Identifier`,
        // so this branch always has a real index and the fallback is unreachable in practice.
        Kind::Keyword => {
            if token.keyword == NOT_A_KEYWORD {
                FIRST_IDENT
            } else {
                bucket(u32::from(token.keyword))
            }
        }
        Kind::Number => FIRST_NUMBER,
        Kind::String => FIRST_STRING,
        Kind::Operator => FIRST_OPERATOR,
        Kind::Terminator => FIRST_TERMINATOR,
        Kind::EndOfInput => FIRST_END,
    }
}

/// The rule with this name, if there is one.
pub fn rule(name: &str) -> Option<&'static Rule> {
    crate::generated::rules::RULES
        .binary_search_by(|candidate| candidate.name.cmp(name))
        .ok()
        .map(|index| &crate::generated::rules::RULES[index])
}

#[cfg(test)]
mod tests {
    use super::{Node, Op, Suggestion, bucket, rule, token_key};
    use crate::generated::rules::{CHILDREN, NODES, PROGRAM, RULES, SYMBOLS};
    use crate::token::{Flags, Kind, Token};

    fn token(kind: Kind, keyword: u16) -> Token {
        Token { kind, flags: Flags::default(), keyword, start: 0, end: 1 }
    }

    #[test]
    fn a_node_is_twenty_four_bytes() {
        assert_eq!(size_of::<Node>(), 24);
    }

    #[test]
    fn the_tables_are_in_range() {
        for node in &NODES {
            match node.op {
                Op::Sequence | Op::Choice => {
                    assert!(node.b > 0, "an empty sequence or choice matches nothing");
                    let end = (node.a + node.b) as usize;
                    assert!(end <= CHILDREN.len());
                    for child in &CHILDREN[node.a as usize..end] {
                        assert!((*child as usize) < NODES.len());
                    }
                }
                Op::Optional | Op::Repeat => assert!((node.a as usize) < NODES.len()),
                Op::Rule => {
                    assert!((node.a as usize) < RULES.len());
                    // The body index the matcher actually jumps to, which is the one thing in the
                    // table that is written twice and so is the one thing that can disagree.
                    assert_eq!(node.b, RULES[node.a as usize].root);
                }
                Op::Symbol => assert!((node.a as usize) < SYMBOLS.len()),
                Op::Keyword => {
                    assert!((node.a as usize) < crate::generated::keywords::KEYWORDS.len())
                }
                _ => {}
            }
        }
        for entry in &RULES {
            assert!((entry.root as usize) < NODES.len());
        }
    }

    #[test]
    fn the_roots_are_there_and_named() {
        assert_eq!(RULES[PROGRAM as usize].name, "Program");
        assert!(rule("Program").is_some());
        assert!(rule("SelectStatement").is_some());
        // Overridden by a matcher, so its written body is dead, but the rule itself is very much
        // reachable and has to be in the table.
        assert!(rule("Identifier").is_some());
        // Not reachable from Program once `Identifier` is overridden, so it should be gone.
        assert!(rule("PlainIdentifier").is_none());
    }

    #[test]
    fn a_repeat_never_wraps_something_that_matches_nothing() {
        // `RepeatMatchProcess` upstream loops while the child succeeds and has no guard for a
        // child that succeeds without consuming, so this is the difference between a table that
        // terminates and one that does not.
        for node in &NODES {
            if node.op == Op::Repeat {
                assert_eq!(NODES[node.a as usize].flags & Node::NULLABLE, 0);
            }
        }
    }

    #[test]
    fn the_filter_only_ever_says_no_to_things_that_could_not_match() {
        // `SELECT` starts a statement, so the root has to admit it.
        let select = crate::generated::keywords::KEYWORDS
            .binary_search_by(|(word, _)| (*word).cmp("select"))
            .expect("select is a keyword");
        let key = token_key(token(Kind::Keyword, select as u16));
        assert!(NODES[RULES[PROGRAM as usize].root as usize].can_start(key));

        // A number does not start a statement, and the root is nullable through
        // `Statement? (';'+ / EndOfInput)`, so this is about the FIRST set and not about whether
        // the parse eventually succeeds on an empty script.
        let number = token_key(token(Kind::Number, u16::MAX));
        let select_rule = rule("SelectStatement").expect("SelectStatement is a rule");
        assert!(!NODES[select_rule.root as usize].can_start(number));
    }

    #[test]
    fn a_token_maps_to_exactly_one_bit() {
        for kind in [
            Kind::Identifier,
            Kind::QuotedIdentifier,
            Kind::Number,
            Kind::String,
            Kind::Operator,
            Kind::Terminator,
            Kind::EndOfInput,
        ] {
            assert_eq!(token_key(token(kind, u16::MAX)).count_ones(), 1, "{kind:?}");
        }
        assert_eq!(token_key(token(Kind::Keyword, 3)).count_ones(), 1);
        assert_eq!(bucket(0).count_ones(), 1);
    }

    #[test]
    fn the_two_derived_answers_match_the_matcher_header() {
        use crate::generated::keywords::{COLUMN_NAME, FUNC_NAME, TYPE_NAME};
        assert_eq!(Suggestion::TypeName.allowed_class(), TYPE_NAME);
        assert_eq!(Suggestion::ScalarFunctionName.allowed_class(), TYPE_NAME | FUNC_NAME);
        assert_eq!(Suggestion::TableFunctionName.allowed_class(), TYPE_NAME | FUNC_NAME);
        assert_eq!(Suggestion::Variable.allowed_class(), COLUMN_NAME);
        assert!(Suggestion::TableName.supports_string_literal());
        assert!(Suggestion::FileName.supports_string_literal());
        assert!(!Suggestion::ColumnName.supports_string_literal());
    }
}
