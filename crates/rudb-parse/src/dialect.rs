//! SQL parser dialects installed in this build.
//!
//! The registry has one entry today because the vendored grammar is DuckDB's grammar. Keeping the
//! name here rather than in the settings layer makes `current_dialect` a lookup whose behavior
//! changes when another parser is registered, not a string special case that has to be replaced.

/// One installed SQL parser dialect.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Dialect {
    name: &'static str,
}

impl Dialect {
    /// The name accepted by `SET current_dialect` and listed by `duckdb_dialects()`.
    #[must_use]
    pub const fn name(self) -> &'static str {
        self.name
    }
}

use crate::token::{Kind, Token};

/// Every SQL parser dialect installed in this build.
pub const DIALECTS: &[Dialect] = &[Dialect { name: "duckdb" }];

/// Finds an installed dialect without regard to identifier case.
#[must_use]
pub fn dialect_named(name: &str) -> Option<Dialect> {
    DIALECTS.iter().copied().find(|dialect| dialect.name.eq_ignore_ascii_case(name))
}

/// Takes out the PostgreSQL words that the vendored grammar does not have and that change nothing
/// in rudb.
///
/// This is a stop gap until `rudb-pgparse` parses the PostgreSQL language (milestone PG4). Each
/// word leaves the token list and the source text stays the same, so the other tokens keep their
/// offsets and an error still points at the right place. Today the one word is `UNLOGGED` in
/// `CREATE UNLOGGED TABLE` and `CREATE UNLOGGED SEQUENCE`. rudb writes every table to its log, and
/// a table that PostgreSQL does not log answers each query the same.
pub fn postgres_tokens(query: &str, tokens: &mut Vec<Token>) {
    let word = |token: &Token, text: &str| {
        matches!(token.kind, Kind::Identifier | Kind::Keyword)
            && token.text(query).eq_ignore_ascii_case(text)
    };
    let mut at = 1;
    while at + 1 < tokens.len() {
        let next = &tokens[at + 1];
        if word(&tokens[at], "unlogged")
            && word(&tokens[at - 1], "create")
            && (word(next, "table") || word(next, "sequence"))
        {
            tokens.remove(at);
        }
        at += 1;
    }
}

#[cfg(test)]
mod tests {
    use super::{DIALECTS, dialect_named, postgres_tokens};

    #[test]
    fn the_vendored_grammar_is_the_one_registered_dialect() {
        assert_eq!(DIALECTS.len(), 1);
        assert_eq!(dialect_named("DUCKDB").map(|dialect| dialect.name()), Some("duckdb"));
        assert_eq!(dialect_named("cypher"), None);
    }

    #[test]
    fn a_postgres_session_reads_create_unlogged_as_create() {
        let query = "CREATE UNLOGGED TABLE t (a INT); SELECT unlogged FROM t";
        let mut tokens = crate::tokenize(query).expect("tokens");
        let before = tokens.len();
        postgres_tokens(query, &mut tokens);
        assert_eq!(tokens.len(), before - 1);
        assert_eq!(tokens[1].text(query), "TABLE");
        assert!(tokens.iter().any(|token| token.text(query) == "unlogged"));
    }
}
