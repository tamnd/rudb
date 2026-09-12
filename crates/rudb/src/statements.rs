//! Splitting a script into the statements it holds.

use rudb_common::Result;
use rudb_parse::{Kind, tokenize};

/// One statement out of a script, and where it started.
///
/// The text runs from the first token to the last, so it excludes the semicolon and any comment
/// that sat before or after the statement, and it keeps a comment that sat inside one. That is the
/// only split that lets the offset mean anything, and a comment between two tokens is something the
/// parser skips anyway. The offset is kept so that a span
/// coming back from the binder can be moved back into the coordinates of the whole script, which is
/// what a shell needs in order to point at the right line.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Statement<'a> {
    sql: &'a str,
    offset: usize,
}

impl<'a> Statement<'a> {
    /// The statement, without its terminator.
    #[must_use]
    pub fn sql(&self) -> &'a str {
        self.sql
    }

    /// The byte offset of the statement in the script it came from.
    #[must_use]
    pub fn offset(&self) -> usize {
        self.offset
    }
}

/// Every statement in a script, in order.
///
/// The split is done by the tokenizer rather than by looking for semicolons, which is the only way
/// to get it right. A semicolon inside a string literal, inside a dollar quoted body or inside a
/// comment is not a statement boundary, and a splitter that reads bytes has to reimplement the
/// tokenizer badly to know that. `rudb_parse::Kind::Terminator` exists for exactly this reason: the
/// tokenizer decides where a statement ends and nothing above it gets a second opinion.
///
/// A script of nothing but comments and whitespace gives back no statements rather than one empty
/// one, so a caller can run what comes back without checking each entry for emptiness.
///
/// Trimming the text to the last token has one visible cost, which is #297. The parser tokenizes the
/// slice again, and a number literal that ends in an exponent marker is at the end of the input there
/// even when it was not at the end of the script, so `SELECT 1e;` refuses where upstream answers a
/// row.
///
/// # Errors
///
/// A tokenizer error, which is an unterminated string or a byte that cannot start a token.
pub fn statements(script: &str) -> Result<Vec<Statement<'_>>> {
    let tokens = tokenize(script)?;
    let mut found = Vec::new();
    let mut start: Option<usize> = None;
    let mut end = 0usize;
    for token in tokens {
        match token.kind {
            Kind::Terminator | Kind::EndOfInput => {
                if let Some(at) = start.take() {
                    found.push(Statement { sql: &script[at..end], offset: at });
                }
            }
            _ => {
                if start.is_none() {
                    start = Some(token.start as usize);
                }
                end = token.end as usize;
            }
        }
    }
    Ok(found)
}

/// Whether a script ends on a statement boundary.
///
/// This is the question a prompt asks between lines. False means the user is in the middle of
/// something and the next line belongs to the same statement, true means what has been typed can be
/// run. Ending on a semicolon is the rule, which is DuckDB's rule and the reason a prompt shows a
/// continuation marker until it sees one.
///
/// A script that does not tokenize is not complete, because the commonest reason for that is a
/// string or a block comment the user has not closed yet, and waiting for the next line is the
/// right answer to that. A script of nothing but whitespace and comments is complete, since there
/// is nothing to wait for.
#[must_use]
pub fn is_complete(script: &str) -> bool {
    let Ok(tokens) = tokenize(script) else {
        return false;
    };
    tokens
        .iter()
        .rev()
        .find(|token| !matches!(token.kind, Kind::EndOfInput))
        .is_none_or(|token| matches!(token.kind, Kind::Terminator))
}

#[cfg(test)]
mod tests {
    use super::{is_complete, statements};

    fn split(script: &str) -> Vec<&str> {
        statements(script).expect("tokenizes").into_iter().map(|found| found.sql()).collect()
    }

    #[test]
    fn one_statement_with_no_terminator_is_a_statement() {
        assert_eq!(split("SELECT 1"), vec!["SELECT 1"]);
    }

    #[test]
    fn a_terminator_ends_a_statement() {
        assert_eq!(split("SELECT 1; SELECT 2;"), vec!["SELECT 1", "SELECT 2"]);
    }

    #[test]
    fn an_empty_script_holds_nothing() {
        assert!(split("").is_empty());
        assert!(split("   \n\t ").is_empty());
        assert!(split(";;;").is_empty());
        assert!(split("-- just a comment\n").is_empty());
        assert!(split("/* and a block one */").is_empty());
    }

    #[test]
    fn a_semicolon_in_a_string_is_not_a_boundary() {
        assert_eq!(split("SELECT ';'"), vec!["SELECT ';'"]);
        assert_eq!(split("SELECT $$a;b$$"), vec!["SELECT $$a;b$$"]);
    }

    #[test]
    fn a_semicolon_in_a_comment_is_not_a_boundary() {
        assert_eq!(split("SELECT 1 -- ; not this one\n"), vec!["SELECT 1"]);
        assert_eq!(split("SELECT /* ; */ 1"), vec!["SELECT /* ; */ 1"]);
    }

    #[test]
    fn comments_around_a_statement_are_not_part_of_it() {
        assert_eq!(split("-- before\nSELECT 1; -- after\n"), vec!["SELECT 1"]);
    }

    #[test]
    fn a_statement_is_complete_when_it_ends_in_a_semicolon() {
        assert!(is_complete("SELECT 1;"));
        assert!(is_complete("SELECT 1; SELECT 2;  \n"));
        assert!(!is_complete("SELECT 1"));
        assert!(!is_complete("SELECT 1; SELECT 2"));
        assert!(!is_complete("CREATE TABLE t ("));
    }

    #[test]
    fn nothing_at_all_is_complete() {
        assert!(is_complete(""));
        assert!(is_complete("  \n "));
        assert!(is_complete("-- a comment\n"));
    }

    #[test]
    fn an_unclosed_string_is_not_complete() {
        assert!(!is_complete("SELECT 'half"));
    }

    #[test]
    fn the_offset_points_back_into_the_script() {
        let script = "SELECT 1;\nSELECT 2;";
        let found = statements(script).expect("tokenizes");
        assert_eq!(found[0].offset(), 0);
        assert_eq!(found[1].offset(), 10);
        assert_eq!(&script[found[1].offset()..], "SELECT 2;");
    }
}
