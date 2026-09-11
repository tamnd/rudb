//! Questions about a statement that do not need a database to answer.
//!
//! Everything here works on text alone. There is no catalog, so no name is resolved and no type is
//! decided, and that is the point: a harness sorting a corpus, a shell deciding whether to keep
//! reading, and a tool counting what the grammar accepts all need answers before they have anywhere
//! to run the statement, and none of them should have to depend on `rudb-parse` to get them.
//!
//! That last part is the whole reason this module exists. `spec/13-client-api.md` says a program
//! embedding rudb depends on this crate and nothing else, and `rudb-compat` reaching into
//! `rudb-parse` for a tokenizer was the counterexample. Every reach it had is answered here.

use rudb_common::{Error, Result};
use rudb_parse::ast::Statement as Parsed;
use rudb_parse::{parse, parse_ast};

/// Whether the rows a statement produces come back in an order it asked for.
///
/// Three answers rather than a boolean, because "we could not tell" is a different thing from "it
/// did not ask" and a caller that folds them together makes the wrong mistake somewhere. A
/// differential harness comparing two engines wants to sort both sides when the order is
/// unspecified and to compare as written when it is declared, and for the third case it has to
/// pick, which is a policy decision that belongs to the harness and not here.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RowOrder {
    /// The statement has a top level `ORDER BY`, so the order is part of the answer.
    Declared,
    /// It parses, it has no top level `ORDER BY`, and so any order is a correct one.
    Unspecified,
    /// It does not parse here, so there is nothing to read the answer off.
    Unknown,
}

/// How the rows of a statement are ordered, as far as the text says.
///
/// Top level only. An `ORDER BY` inside a subquery does not count, because it does not survive into
/// the outer result and a caller that treated it as an order would be comparing against an order
/// nothing promised. `ORDER BY ALL` counts, since it is an order clause spelled shorter.
///
/// A statement that returns no rows is [`RowOrder::Unspecified`], which is true in the only sense
/// that matters: there is no order to preserve.
#[must_use]
pub fn row_order(sql: &str) -> RowOrder {
    let Ok(ast) = parse_ast(sql) else {
        return RowOrder::Unknown;
    };
    let declared = ast.statements.iter().any(|statement| match statement {
        Parsed::Query(at) => {
            let query = ast.query(*at);
            query.order_by_all || !query.order_by.is_empty()
        }
        // Nothing else returns rows, so nothing else has an order to preserve.
        _ => false,
    });
    if declared { RowOrder::Declared } else { RowOrder::Unspecified }
}

/// Whether the grammar accepts this statement.
///
/// The grammar and nothing above it. A statement this accepts may still fail to run, because the
/// names in it may not exist and the types may not work out, and a statement this accepts may not
/// even build an AST yet, because the grammar is vendored whole from DuckDB while the AST is built
/// one statement at a time.
///
/// That gap is the reason this is a separate call from [`crate::Database::prepare`] rather than
/// something a caller infers from one. A conformance harness asking which of DuckDB's dialect we
/// accept is asking about the grammar, and answering with the AST instead would report a hole in
/// the dialect wherever there is a hole in the AST, which is a different and much larger number.
///
/// # Errors
///
/// A tokenizer error or a parse error, carrying the position in the text.
pub fn accepts(sql: &str) -> Result<()> {
    parse(sql).map(|_| ())
}

/// Whether the grammar accepts this statement, as a plain yes or no.
///
/// For a caller counting how much of a corpus parses, where the message is not read.
#[must_use]
pub fn parses(sql: &str) -> bool {
    accepts(sql).is_ok()
}

/// The text split into statements, with text that does not tokenize handed back whole.
///
/// [`crate::statements`] is the same split and refuses text it cannot tokenize, which is the right
/// answer for a shell, since the commonest reason is a string the user has not closed. It is the
/// wrong answer for a harness reading a file of other people's SQL, whose job is to hand each
/// statement to both engines and see what they say rather than to decide in advance that something
/// is not SQL. This is that second reading.
///
/// # Errors
///
/// Never. It returns a `Result` so that a caller can use it where [`crate::statements`] would go,
/// and so that a later reason to refuse has somewhere to live.
pub fn split(text: &str) -> Result<Vec<String>> {
    let Ok(found) = crate::statements(text) else {
        let trimmed = text.trim();
        return Ok(if trimmed.is_empty() { Vec::new() } else { vec![trimmed.to_string()] });
    };
    Ok(found.into_iter().map(|statement| statement.sql().to_string()).collect())
}

/// Where in the text an error is about, as a line and a column, both counting from one.
///
/// Byte offsets are what the parser carries, because that is what slicing wants, and a line and a
/// column are what a person reads. Doing the conversion here rather than in each caller means the
/// three of them agree about tabs and about what happens at the very end of the text.
///
/// The column counts characters rather than bytes, so a multi byte character is one column, which
/// is what an editor shows. A position past the end of the text is the position just after the last
/// character, since an error about a statement that ended too early is about the end.
#[must_use]
pub fn line_and_column(text: &str, offset: usize) -> (usize, usize) {
    let upto = &text[..offset.min(text.len())];
    let line = upto.bytes().filter(|&b| b == b'\n').count() + 1;
    let column = upto.rsplit('\n').next().unwrap_or("").chars().count() + 1;
    (line, column)
}

/// The span of an error, as a line and a column into the statement it came from.
///
/// `None` when the error does not say where, which is most of the errors that are not about syntax.
#[must_use]
pub fn where_it_happened(sql: &str, error: &Error) -> Option<(usize, usize)> {
    error.span().map(|span| line_and_column(sql, span.start as usize))
}

#[cfg(test)]
mod tests {
    use rudb_common::Error;

    use super::{RowOrder, accepts, line_and_column, parses, row_order, split, where_it_happened};

    #[test]
    fn a_top_level_order_by_is_a_declared_order() {
        assert_eq!(row_order("SELECT x FROM t ORDER BY x"), RowOrder::Declared);
        assert_eq!(row_order("SELECT * FROM t ORDER BY ALL"), RowOrder::Declared);
    }

    #[test]
    fn a_query_with_no_order_by_promises_nothing_about_the_order() {
        assert_eq!(row_order("SELECT x FROM t"), RowOrder::Unspecified);
        assert_eq!(row_order("SELECT 1"), RowOrder::Unspecified);
    }

    #[test]
    fn an_order_by_inside_a_subquery_does_not_survive_into_the_outer_result() {
        // So it is not an order the outer query promised, and a caller comparing against it would
        // be comparing against something nothing said.
        assert_eq!(
            row_order("SELECT x FROM (SELECT x FROM t ORDER BY x) AS inner_query"),
            RowOrder::Unspecified
        );
    }

    #[test]
    fn a_statement_that_returns_no_rows_has_no_order_to_preserve() {
        assert_eq!(row_order("CREATE TABLE t (x INTEGER)"), RowOrder::Unspecified);
        assert_eq!(row_order("INSERT INTO t VALUES (1)"), RowOrder::Unspecified);
    }

    #[test]
    fn text_that_does_not_parse_here_says_it_does_not_know() {
        assert_eq!(row_order("SELECT FROM WHERE"), RowOrder::Unknown);
        assert_eq!(row_order("this is not sql at all"), RowOrder::Unknown);
    }

    #[test]
    fn the_grammar_accepts_more_than_the_ast_builds() {
        // `MERGE INTO` is in the vendored grammar and there is no AST for it, and that gap is
        // exactly why acceptance is a separate question from preparing a statement.
        assert!(parses("SELECT x FROM t"));
        assert!(parses("MERGE INTO t USING s ON t.x = s.x WHEN MATCHED THEN DELETE"));
        assert_eq!(
            row_order("MERGE INTO t USING s ON t.x = s.x WHEN MATCHED THEN DELETE"),
            RowOrder::Unknown
        );
    }

    #[test]
    fn something_that_is_not_sql_is_rejected_with_a_parser_error() {
        let error = accepts("SELECT FROM WHERE").expect_err("that is not valid SQL");
        assert_eq!(error.code().duckdb_name(), "Parser Error");
        assert!(!parses("SELECT FROM WHERE"));
    }

    #[test]
    fn a_file_splits_into_its_statements() {
        assert_eq!(split("SELECT 1; SELECT 2;").unwrap(), vec!["SELECT 1", "SELECT 2"]);
        assert_eq!(split("SELECT ';'").unwrap(), vec!["SELECT ';'"]);
        assert!(split("-- nothing but a comment\n").unwrap().is_empty());
        assert!(split("   ").unwrap().is_empty());
    }

    #[test]
    fn text_that_does_not_tokenize_comes_back_whole_rather_than_being_refused() {
        // A harness reading somebody else's SQL hands it to both engines and reports what they say.
        // Deciding here that it is not SQL is the harness answering its own question.
        assert_eq!(split("SELECT 'unclosed").unwrap(), vec!["SELECT 'unclosed"]);
    }

    #[test]
    fn a_byte_offset_becomes_the_line_and_column_a_person_reads() {
        let text = "SELECT 1\nFROM t\nWHERE x";
        assert_eq!(line_and_column(text, 0), (1, 1));
        assert_eq!(line_and_column(text, 7), (1, 8));
        assert_eq!(line_and_column(text, 9), (2, 1));
        assert_eq!(line_and_column(text, 16), (3, 1));
        // Past the end is the position just after the last character, because an error about a
        // statement that ended too early is about the end.
        assert_eq!(line_and_column(text, 9_999), (3, 8));
    }

    #[test]
    fn a_column_counts_characters_rather_than_bytes() {
        // Four bytes, one character, so the next column is two and not five.
        assert_eq!(line_and_column("\u{1f600}x", 4), (1, 2));
    }

    #[test]
    fn an_error_with_no_span_says_nothing_about_where() {
        assert_eq!(where_it_happened("SELECT 1", &Error::internal("no span on this one")), None);
    }
}
