//! The SQL statements on prepared statements: `PREPARE`, `EXECUTE` and `DEALLOCATE`.
//!
//! A statement of `PREPARE` and a named statement of `Parse` share one namespace in PostgreSQL,
//! so a `Bind` can use a statement of `PREPARE` and `DEALLOCATE` removes a statement of `Parse`.
//! The server runs these statements itself on the statements of the session and does not give
//! them to the engine. This module reads them with the grammar of `gram.y`. A statement that this
//! reader cannot read goes to the engine, which gives the syntax error.

use super::setting::{Token, loose_spanned};

/// A statement on the prepared statements of the session.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum Named {
    /// `PREPARE name [ ( type [, ...] ) ] AS statement`.
    Prepare {
        name: String,
        /// The byte range of each type in the statement.
        types: Vec<(usize, usize)>,
        /// The byte offset of the statement to prepare.
        query: usize,
    },
    /// `EXECUTE name [ ( value [, ...] ) ]`.
    Execute {
        name: String,
        /// The byte range of each value in the statement.
        values: Vec<(usize, usize)>,
    },
    /// `DEALLOCATE [ PREPARE ] name`, or `DEALLOCATE ALL` with `None`.
    Deallocate(Option<String>),
}

/// Reads a statement on the prepared statements, or gives `None` for another statement.
pub(super) fn parse(sql: &str) -> Option<Named> {
    let head = sql.trim_start().as_bytes();
    let starts = |word: &str| {
        head.get(..word.len()).is_some_and(|h| h.eq_ignore_ascii_case(word.as_bytes()))
    };
    if !["prepare", "execute", "deallocate"].iter().any(|w| starts(w)) {
        return None;
    }
    if starts("prepare") {
        // Only the head up to `AS` is read here, since the statement has the full grammar.
        let end = head_end(sql)?;
        let (tokens, offsets) = loose_spanned(&sql[..end])?;
        let mut p = Reader { tokens: &tokens, offsets: &offsets, end, at: 1 };
        // `PREPARE TRANSACTION` is a statement of two-phase commit.
        if p.peek()?.is("transaction") {
            return None;
        }
        let name = p.name()?;
        let types = if p.peek() == Some(&Token::Punct('(')) { p.list()? } else { Vec::new() };
        if !p.eat("as") || p.at != tokens.len() {
            return None;
        }
        let query = sql[end..].trim_start();
        if query.trim_end_matches([';', ' ', '\t', '\r', '\n']).is_empty() {
            return None;
        }
        return Some(Named::Prepare { name, types, query: sql.len() - query.len() });
    }
    let (mut tokens, mut offsets) = loose_spanned(sql)?;
    while tokens.last() == Some(&Token::Punct(';')) {
        tokens.pop();
        offsets.pop();
    }
    let mut p = Reader { tokens: &tokens, offsets: &offsets, end: sql.len(), at: 1 };
    let named = match tokens.first()? {
        first if first.is("execute") => {
            let name = p.name()?;
            let values = if p.peek() == Some(&Token::Punct('(')) { p.list()? } else { Vec::new() };
            Named::Execute { name, values }
        }
        first if first.is("deallocate") => {
            p.eat("prepare");
            if p.eat("all") { Named::Deallocate(None) } else { Named::Deallocate(Some(p.name()?)) }
        }
        _ => return None,
    };
    (p.at == tokens.len()).then_some(named)
}

/// The end of the first word `AS` of a `PREPARE` outside of parentheses, which ends its head. A
/// quoted name or a string can hold the word, so the quotes are skipped.
fn head_end(sql: &str) -> Option<usize> {
    let bytes = sql.as_bytes();
    let mut depth = 0usize;
    let mut at = 0;
    while at < bytes.len() {
        let byte = bytes[at];
        if byte == b'"' || byte == b'\'' {
            at += 1;
            while at < bytes.len() && bytes[at] != byte {
                at += 1;
            }
            at += 1;
        } else if byte.is_ascii_alphabetic() || byte == b'_' {
            let start = at;
            while at < bytes.len() && (bytes[at].is_ascii_alphanumeric() || bytes[at] == b'_') {
                at += 1;
            }
            if depth == 0 && sql[start..at].eq_ignore_ascii_case("as") {
                return Some(at);
            }
        } else {
            match byte {
                b'(' => depth += 1,
                b')' => depth = depth.checked_sub(1)?,
                _ => {}
            }
            at += 1;
        }
    }
    None
}

/// The first word of `query` when it is not a statement that `PREPARE` takes, which is the
/// `PreparableStmt` of `gram.y`.
pub(super) fn not_preparable(query: &str) -> Option<&str> {
    let query = query.trim_start();
    if query.starts_with('(') {
        return None;
    }
    let end = query.find(|c: char| !c.is_alphanumeric() && c != '_').unwrap_or(query.len());
    let word = &query[..end];
    let known = ["select", "values", "table", "with", "insert", "update", "delete", "merge"];
    (!known.iter().any(|known| word.eq_ignore_ascii_case(known))).then_some(word)
}

struct Reader<'a> {
    tokens: &'a [Token],
    offsets: &'a [usize],
    /// The end of the statement, for the end of the last token.
    end: usize,
    at: usize,
}

impl Reader<'_> {
    fn peek(&self) -> Option<&Token> {
        self.tokens.get(self.at)
    }

    fn eat(&mut self, word: &str) -> bool {
        let found = self.peek().is_some_and(|token| token.is(word));
        self.at += usize::from(found);
        found
    }

    fn name(&mut self) -> Option<String> {
        let Token::Word { text, .. } = self.peek()? else {
            return None;
        };
        let name = text.clone();
        self.at += 1;
        Some(name)
    }

    /// A list in parentheses with at least one item, as the byte range of each item. A comma in
    /// inner parentheses or brackets does not end an item.
    fn list(&mut self) -> Option<Vec<(usize, usize)>> {
        self.at += 1;
        let mut items = Vec::new();
        let mut depth = 0usize;
        let mut start = self.at;
        loop {
            let token = self.peek()?;
            match token {
                Token::Punct('(' | '[') => depth += 1,
                Token::Punct(')') if depth == 0 => {
                    items.push(self.range(start)?);
                    self.at += 1;
                    return Some(items);
                }
                Token::Punct(')' | ']') => depth = depth.checked_sub(1)?,
                Token::Punct(',') if depth == 0 => {
                    items.push(self.range(start)?);
                    start = self.at + 1;
                }
                _ => {}
            }
            self.at += 1;
        }
    }

    /// The byte range of the tokens from `start` up to the token at the place of the reader, with
    /// no space at its end. `None` when there are no tokens.
    fn range(&self, start: usize) -> Option<(usize, usize)> {
        if start >= self.at {
            return None;
        }
        let end = self.offsets.get(self.at).copied().unwrap_or(self.end);
        Some((self.offsets[start], end))
    }
}

#[cfg(test)]
mod tests {
    use super::{Named, not_preparable, parse};

    #[test]
    fn the_statements_are_read_as_gram_y_reads_them() {
        let sql = "PREPARE Q (int, numeric(10, 2)) AS SELECT $1 AS a";
        let Some(Named::Prepare { name, types, query }) = parse(sql) else {
            panic!("not read: {sql}");
        };
        assert_eq!(name, "q");
        let types: Vec<&str> = types.iter().map(|(s, e)| sql[*s..*e].trim_end()).collect();
        assert_eq!(types, ["int", "numeric(10, 2)"]);
        assert_eq!(&sql[query..], "SELECT $1 AS a");
        let sql = "execute \"Q\" (1 + 2, f(3, 4), '{1,2}'::int[]);";
        let Some(Named::Execute { name, values }) = parse(sql) else {
            panic!("not read: {sql}");
        };
        assert_eq!(name, "Q");
        let values: Vec<&str> = values.iter().map(|(s, e)| sql[*s..*e].trim_end()).collect();
        assert_eq!(values, ["1 + 2", "f(3, 4)", "'{1,2}'::int[]"]);
        assert_eq!(parse("execute q"), Some(Named::Execute { name: "q".into(), values: vec![] }));
        assert_eq!(parse("deallocate all"), Some(Named::Deallocate(None)));
        assert_eq!(parse("DEALLOCATE PREPARE all"), Some(Named::Deallocate(None)));
        assert_eq!(parse("deallocate prepare p"), Some(Named::Deallocate(Some("p".into()))));
        assert_eq!(parse("deallocate p q"), None);
        assert_eq!(parse("execute q()"), None);
        assert_eq!(parse("prepare transaction 'x'"), None);
        assert_eq!(parse("prepare q as"), None);
        assert_eq!(parse("select 1"), None);
        assert_eq!(not_preparable(" create table t (a int)"), Some("create"));
        assert_eq!(not_preparable("(select 1)"), None);
        assert_eq!(not_preparable("insert into t values (1)"), None);
    }
}
