//! What names are visible, and what they resolve to.
//!
//! A scope is a flat list of visible columns in the order they would come out of a `SELECT *`. It
//! is flat rather than a map because the list is short, because the order is part of the answer,
//! and because ambiguity is a question about the whole list rather than about one bucket of it.
//!
//! Every entry carries the table name it came in under, which is the alias if there was one and the
//! table's own name if there was not. That is the name `t.x` matches against and the name an error
//! message should use, and it is deliberately not the catalog name: after `FROM hits AS h` there is
//! no `hits` to refer to, which is SQL's rule and not ours.

use rudb_catalog::same_name;
use rudb_common::{Error, LogicalType, Result};
use rudb_plan::ColumnBinding;

/// One visible column.
#[derive(Debug, Clone)]
pub(crate) struct Visible {
    /// The table name it is reachable through, empty for a column of no table.
    pub(crate) table: String,
    /// The column name.
    pub(crate) name: String,
    /// Where it comes from in the plan.
    pub(crate) binding: ColumnBinding,
    /// What it is.
    pub(crate) ty: LogicalType,
    /// Whether the column it came from refuses nulls.
    ///
    /// Only `DESCRIBE` reads this, and only to fill the `null` column with `NO` or `YES`. It is
    /// carried on the scope rather than asked of the plan because the question is about where a
    /// column came from and the scope is the only thing that still knows: by the time a projection
    /// is a node, a column that is passed straight through and one that is computed look the same.
    ///
    /// A column that is not a plain reference is nullable whatever it was built from, which is
    /// also what the reference binary says. `DESCRIBE SELECT * FROM t` keeps `NO` on a `NOT NULL`
    /// column and `DESCRIBE SELECT c + 0 FROM t` does not.
    pub(crate) not_null: bool,
}

/// The columns a name can resolve against.
#[derive(Debug, Clone, Default)]
pub(crate) struct Scope {
    pub(crate) columns: Vec<Visible>,
}

impl Scope {
    /// A scope with nothing in it, which is what `SELECT 1` binds against.
    pub(crate) fn empty() -> Self {
        Self { columns: Vec::new() }
    }

    /// Everything on the left followed by everything on the right, which is what a join sees.
    pub(crate) fn concat(mut self, other: Self) -> Self {
        self.columns.extend(other.columns);
        self
    }

    pub(crate) fn push(&mut self, column: Visible) {
        self.columns.push(column);
    }

    pub(crate) fn len(&self) -> usize {
        self.columns.len()
    }

    /// Resolves a written name to one column.
    ///
    /// One part is a column name and it has to be unique across every table in scope. Two parts are
    /// a table and a column. Three and four parts have a schema and a catalog in front, and they
    /// are matched against the table part only, because a table in scope has one name here and the
    /// qualification is decoration once it is in the `FROM` clause.
    ///
    /// # Errors
    ///
    /// If nothing matches, or if one part matches more than one column. The messages are DuckDB's.
    pub(crate) fn resolve(&self, parts: &[&str]) -> Result<&Visible> {
        let (table, column) = match parts {
            [column] => (None, *column),
            [table, column] => (Some(*table), *column),
            [_, table, column] | [_, _, table, column] => (Some(*table), *column),
            _ => {
                return Err(Error::binder(format!(
                    "Referenced column \"{}\" has too many parts to be a column name",
                    parts.join(".")
                )));
            }
        };
        let matched: Vec<&Visible> = self
            .columns
            .iter()
            .filter(|held| {
                same_name(&held.name, column)
                    && table.is_none_or(|table| same_name(&held.table, table))
            })
            .collect();
        match matched.as_slice() {
            [one] => Ok(one),
            [] => Err(self.not_found(table, column)),
            many => {
                let candidates: Vec<String> =
                    many.iter().map(|held| format!("{}.{}", held.table, held.name)).collect();
                Err(Error::binder(format!(
                    "Ambiguous reference to column name \"{column}\" (use: \"{}\")",
                    candidates.join("\" or \"")
                )))
            }
        }
    }

    /// The columns a star expands to.
    ///
    /// # Errors
    ///
    /// If the qualifier names no table in scope, or if there is nothing in scope at all, which is
    /// `SELECT *` with no `FROM` clause and is an error rather than zero columns.
    pub(crate) fn star(&self, qualifier: Option<&str>) -> Result<Vec<&Visible>> {
        let matched: Vec<&Visible> = match qualifier {
            None => self.columns.iter().collect(),
            Some(table) => {
                self.columns.iter().filter(|held| same_name(&held.table, table)).collect()
            }
        };
        if matched.is_empty() {
            return Err(match qualifier {
                Some(table) => {
                    Error::binder(format!("Referenced table \"{table}\" not found in FROM clause!"))
                }
                None => Error::binder("* is not allowed in a query without a FROM clause"),
            });
        }
        Ok(matched)
    }

    /// Renames every column's table, which is what an alias on a subquery or a table does.
    pub(crate) fn relabel(&mut self, table: &str) {
        for column in &mut self.columns {
            column.table = table.to_string();
        }
    }

    /// Replaces the column names, which is what `AS t(a, b)` does.
    ///
    /// # Errors
    ///
    /// If there are more names than columns, which DuckDB reports rather than ignoring.
    pub(crate) fn rename(&mut self, names: &[&str], what: &str) -> Result<()> {
        if names.len() > self.columns.len() {
            return Err(Error::binder(format!(
                "table \"{what}\" has {} columns available but {} columns specified",
                self.columns.len(),
                names.len()
            )));
        }
        for (column, name) in self.columns.iter_mut().zip(names) {
            column.name = (*name).to_string();
        }
        Ok(())
    }

    /// Drops the column at `position`, which is what `USING` does to the right side's copy.
    pub(crate) fn remove(&mut self, position: usize) {
        self.columns.remove(position);
    }

    /// Where a column of that name sits, if exactly one does.
    pub(crate) fn position_of(&self, table: Option<&str>, name: &str) -> Option<usize> {
        let mut found = None;
        for (at, held) in self.columns.iter().enumerate() {
            if same_name(&held.name, name)
                && table.is_none_or(|table| same_name(&held.table, table))
            {
                if found.is_some() {
                    return None;
                }
                found = Some(at);
            }
        }
        found
    }

    fn not_found(&self, table: Option<&str>, column: &str) -> Error {
        match table {
            Some(table) if self.columns.iter().all(|held| !same_name(&held.table, table)) => {
                Error::binder(format!("Referenced table \"{table}\" not found in FROM clause!"))
            }
            Some(table) => Error::binder(format!(
                "Referenced column \"{column}\" not found in table \"{table}\"!"
            )),
            None => Error::binder(format!(
                "Referenced column \"{column}\" not found in FROM clause!{}",
                self.candidates()
            )),
        }
    }

    /// The `Candidate bindings:` part of a complaint about a name that is not here, empty when
    /// there is nothing in scope to suggest.
    ///
    /// On its own line in the binary and on the same line here, because an error is one line here
    /// and the sentence before it is the part anybody matches on.
    pub(crate) fn candidates(&self) -> String {
        let candidates: Vec<&str> = self.columns.iter().map(|held| held.name.as_str()).collect();
        if candidates.is_empty() {
            String::new()
        } else {
            format!(" Candidate bindings: \"{}\"", candidates.join("\", \""))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scope() -> Scope {
        let mut scope = Scope::empty();
        scope.push(Visible {
            table: "hits".into(),
            name: "UserID".into(),
            binding: ColumnBinding::new(0, 0),
            ty: LogicalType::BigInt,
            not_null: false,
        });
        scope.push(Visible {
            table: "hits".into(),
            name: "url".into(),
            binding: ColumnBinding::new(0, 1),
            ty: LogicalType::Varchar,
            not_null: false,
        });
        scope.push(Visible {
            table: "visits".into(),
            name: "url".into(),
            binding: ColumnBinding::new(1, 0),
            ty: LogicalType::Varchar,
            not_null: false,
        });
        scope
    }

    #[test]
    fn a_unique_name_resolves_without_a_table() {
        let scope = scope();
        let found = scope.resolve(&["userid"]).expect("one column is called that");
        assert_eq!(found.binding, ColumnBinding::new(0, 0));
    }

    #[test]
    fn a_name_in_two_tables_needs_the_table() {
        let scope = scope();
        let error = scope.resolve(&["url"]).expect_err("two columns are called url");
        assert!(error.message().contains("Ambiguous"), "{error}");
        let found = scope.resolve(&["visits", "url"]).expect("qualified");
        assert_eq!(found.binding, ColumnBinding::new(1, 0));
    }

    #[test]
    fn a_name_that_is_not_there_lists_what_is() {
        let error = scope().resolve(&["nope"]).expect_err("no such column");
        assert!(error.message().contains("not found in FROM clause"), "{error}");
        assert!(error.message().contains("UserID"), "the message should say what is there");
    }

    #[test]
    fn a_table_that_is_not_there_says_that_rather_than_naming_the_column() {
        let error = scope().resolve(&["nope", "url"]).expect_err("no such table");
        assert!(error.message().contains("Referenced table \"nope\""), "{error}");
    }

    #[test]
    fn a_star_expands_in_order_and_a_qualified_one_expands_to_its_table() {
        let scope = scope();
        let all = scope.star(None).expect("three columns");
        assert_eq!(all.len(), 3);
        assert_eq!(all[0].name, "UserID");
        let one = scope.star(Some("VISITS")).expect("one column, case insensitively");
        assert_eq!(one.len(), 1);
        assert_eq!(one[0].binding, ColumnBinding::new(1, 0));
    }

    #[test]
    fn a_qualified_name_ignores_the_schema_in_front_of_it() {
        let scope = scope();
        let found = scope.resolve(&["memory", "main", "hits", "UserID"]).expect("four parts");
        assert_eq!(found.binding, ColumnBinding::new(0, 0));
    }
}
