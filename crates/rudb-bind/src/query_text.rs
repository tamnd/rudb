//! `query` and `query_table`, the two table functions whose rows are those of a query they are
//! handed rather than of anything they read themselves.
//!
//! `query('SELECT 42')` parses its argument and binds it the way a subquery written there would be,
//! and refuses anything that is not one `SELECT`. `query_table(t)` and `query_table([t, u])` read
//! one table or several, stacked with `UNION ALL`, or with `UNION ALL BY NAME` when the second
//! argument is true. A name that looks like a path is read as a file, the way `FROM 'x.parquet'`
//! is. Both take a bare name as the string it spells, which is what lets a macro hand
//! `query_table` its parameter.

use rudb_common::{Error, LogicalType, Result, Value};
use rudb_parse::ast::{self, LiteralKind};
use rudb_parse::{Ast, NONE, parse_ast_with_case};
use rudb_plan::NodeRef;

use crate::binder::Binder;
use crate::scope::Scope;

impl Binder<'_> {
    /// The rows of a call to `query` or `query_table`, or `None` if the call is to neither.
    pub(crate) fn query_function(
        &mut self,
        ast: &Ast,
        called: &str,
        args: ast::Slice,
        alias: ast::StrRef,
        columns: ast::Slice,
    ) -> Result<Option<(NodeRef, Scope)>> {
        let table = if called.eq_ignore_ascii_case("query") {
            false
        } else if called.eq_ignore_ascii_case("query_table") {
            true
        } else {
            return Ok(None);
        };
        let written = ast.target_list(args).to_vec();
        let empty = Scope::empty();
        let previous = std::mem::replace(&mut self.clause, "table function arguments");
        let identifiers = std::mem::replace(&mut self.identifiers_as_strings, true);
        let mut values = Vec::new();
        let mut spelled = Vec::new();
        let mut failed = None;
        for argument in &written {
            let bound = match self.bind_expr(ast, argument.expr, &empty) {
                Ok(bound) => bound,
                Err(error) => {
                    failed = Some(error);
                    break;
                }
            };
            let ty = self.plan().expr_type(bound).clone();
            spelled.push(match ast.expr(argument.expr) {
                ast::Expr::Literal { kind: LiteralKind::String, .. } | ast::Expr::Column { .. }
                    if ty == LogicalType::Varchar =>
                {
                    "STRING_LITERAL".to_string()
                }
                _ if ty == LogicalType::Null => "\"NULL\"".to_string(),
                _ => ty.to_string(),
            });
            match crate::fold::value_of(self.plan(), bound) {
                Ok(Some(value)) => values.push((value, ty)),
                Ok(None) => {
                    failed = Some(Error::binder(format!(
                        "Table function cannot contain subqueries or non-constant arguments: {}",
                        rudb_parse::deparse::expression(ast, argument.expr)
                    )));
                    break;
                }
                Err(error) => {
                    failed = Some(error);
                    break;
                }
            }
        }
        self.identifiers_as_strings = identifiers;
        self.clause = previous;
        if let Some(error) = failed {
            return Err(error);
        }
        let text = if table {
            query_table_text(&values, &spelled)?
        } else {
            query_text(&values, &spelled)?
        };
        let parsed = parse_ast_with_case(&text, self.semantics.identifier_case())?;
        let query = match parsed.statements[..] {
            [ast::Statement::Query(query)] => query,
            _ => return Err(Error::parser("Expected a single SELECT statement")),
        };
        // What the text binds to is placed where the call was written, since the text is not what
        // the user wrote and a caret into it would point at the wrong words.
        let outer = self.pinned_span.replace(self.current_span);
        let bound = self.bind_query(&parsed, query);
        self.pinned_span = outer;
        let (node, mut scope) = bound?;
        let label = if alias == NONE {
            "unnamed_subquery".to_string()
        } else {
            ast.string(alias).to_string()
        };
        scope.relabel(&label);
        if !columns.is_empty() {
            let names: Vec<&str> = ast.name(columns).collect();
            scope.rename(&names, &label)?;
        }
        Ok(Some((node, scope)))
    }
}

/// The text `query` runs: its one argument, which has to be a string.
fn query_text(values: &[(Value, LogicalType)], spelled: &[String]) -> Result<String> {
    match values {
        [(Value::Varchar(text), _)] => Ok(text.clone()),
        [(Value::Null, LogicalType::Varchar | LogicalType::Null)] => {
            Err(Error::binder("Cannot use NULL as function argument"))
        }
        _ => Err(Error::binder(format!(
            "No function matches the given name and argument types 'query({})'. You might need \
             to add explicit type casts.\n\tCandidate functions:\n\t\"query\"(VARCHAR)\n",
            spelled.join(", ")
        ))),
    }
}

/// The text `query_table` runs: a `FROM` for each name it was given, stacked with `UNION ALL`.
fn query_table_text(values: &[(Value, LogicalType)], spelled: &[String]) -> Result<String> {
    let names =
        matches!(values.first().map(|(_, ty)| ty), Some(LogicalType::Varchar | LogicalType::Null))
            || matches!(values.first().map(|(_, ty)| ty), Some(LogicalType::List(element))
            if matches!(**element, LogicalType::Varchar | LogicalType::Null));
    let flag = match values.get(1) {
        None => Some(false),
        Some((Value::Boolean(flag), _)) => Some(*flag),
        Some((Value::Null, LogicalType::Boolean)) => Some(false),
        Some(_) => None,
    };
    let (true, Some(by_name), 1..=2) = (names, flag, values.len()) else {
        return Err(Error::binder(format!(
            "No function matches the given name and argument types 'query_table({})'. You might \
             need to add explicit type casts.\n\tCandidate functions:\n\t\"query_table\"(VARCHAR)\
             \n\t\"query_table\"(VARCHAR[])\n\t\"query_table\"(VARCHAR[], BOOLEAN)\n",
            spelled.join(", ")
        )));
    };
    let tables: Vec<&str> = match &values[0].0 {
        Value::Null => return Err(Error::binder("Cannot use NULL as function argument")),
        Value::Varchar(name) => vec![name.as_str()],
        Value::List { values: items, .. } if items.is_empty() => {
            return Err(Error::invalid_input("Input list is empty"));
        }
        Value::List { values: items, .. } => items
            .iter()
            .filter_map(|item| match item {
                Value::Varchar(name) => Some(name.as_str()),
                _ => None,
            })
            .collect(),
        _ => Vec::new(),
    };
    if tables.is_empty() {
        return Err(Error::invalid_input("Expected a table or a list with tables as input"));
    }
    let froms: Vec<String> = tables.iter().map(|name| source(name)).collect::<Result<_>>()?;
    let between = if by_name { " UNION ALL BY NAME " } else { " UNION ALL " };
    Ok(froms.join(between))
}

/// The endings of a name the pin reads as a file rather than a table, through the same replacement
/// scan that reads `FROM 'x.csv'`. A compressed file keeps its own ending after these.
const FILES: &[&str] = &[".csv", ".tsv", ".parquet", ".json", ".jsonl", ".ndjson", ".txt"];

/// `FROM` and the name, written so that it is read the way the pin reads it: a path, a URL or a name
/// with a file's ending as the file it names, and anything else as a table name in up to three
/// parts, each one quoted so that `(SELECT 17 + 25)` is the name of a table and not a query.
fn source(name: &str) -> Result<String> {
    let file = FILES.iter().any(|extension| {
        let lowered = name.to_ascii_lowercase();
        lowered.ends_with(extension) || lowered.contains(&format!("{extension}."))
    });
    if !name.contains('"') && (file || name.contains('/') || name.contains('\\')) {
        return Ok(format!("FROM '{}'", name.replace('\'', "''")));
    }
    let parts = components(name)?;
    if parts.iter().any(String::is_empty) {
        return Err(Error::parser("syntax error at or near \"FROM\""));
    }
    let quoted: Vec<String> =
        parts.iter().map(|part| format!("\"{}\"", part.replace('"', "\"\""))).collect();
    Ok(format!("FROM {}", quoted.join(".")))
}

/// The parts of a name written in a string, split at each dot outside double quotes, the way the
/// pin splits one. A quote may only open a part, and has to close it.
fn components(name: &str) -> Result<Vec<String>> {
    let unexpected = || {
        Error::parser(format!(
            "Unexpected quote in the middle of a qualified name component! (input: {name})"
        ))
    };
    let mut parts = Vec::new();
    let mut part = String::new();
    let mut chars = name.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            '"' if part.is_empty() => {
                loop {
                    match chars.next() {
                        Some('"') if chars.peek() == Some(&'"') => {
                            chars.next();
                            part.push('"');
                        }
                        Some('"') => break,
                        Some(c) => part.push(c),
                        None => {
                            return Err(Error::parser(format!(
                                "Unterminated quote in qualified name! (input: {name})"
                            )));
                        }
                    }
                }
                match chars.next() {
                    Some('.') => parts.push(std::mem::take(&mut part)),
                    Some(_) => return Err(unexpected()),
                    None => {}
                }
            }
            '"' => return Err(unexpected()),
            '.' => parts.push(std::mem::take(&mut part)),
            c => part.push(c),
        }
    }
    parts.push(part);
    Ok(parts)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_name_is_split_at_dots_outside_quotes() {
        assert_eq!(components("a.b").expect("two"), ["a", "b"]);
        assert_eq!(components("\"a/b\".t").expect("quoted"), ["a/b", "t"]);
        assert_eq!(components("\"a.b\"").expect("one"), ["a.b"]);
        assert_eq!(components("(SELECT 17 + 25)").expect("odd"), ["(SELECT 17 + 25)"]);
        let error = components("FROM query(\"x\")").expect_err("mid quote");
        assert!(error.to_string().contains("Unexpected quote in the middle"));
    }
}
