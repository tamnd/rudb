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

use rudb_common::{Error, Field, IdentifierCompare, LogicalType, Origin, Result, SqlState};
use rudb_plan::{ColumnBinding, NodeRef};

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
    /// `PRI` or `UNI` when the column it came from is in a key of its table, carried the same way
    /// and for the same reader as `not_null`.
    pub(crate) key: Option<&'static str>,
    /// The SQL of the `DEFAULT` of the column it came from, carried the same way as `key`.
    pub(crate) default: Option<String>,
    /// The table column that the column reads with no change, carried the same way as `key`. A
    /// PostgreSQL session sends it in `RowDescription`.
    pub(crate) origin: Option<Origin>,
    /// Whether only a name with the table in front of it reaches the column, which is what the
    /// `excluded` of an `ON CONFLICT DO UPDATE` is: a bare name there means the held row's column.
    pub(crate) qualified: bool,
    /// A second name the column answers to when no column has the one asked for.
    ///
    /// Only `range` and `generate_series` set it. `FROM range(3) r` names the one column `r`, the
    /// way PostgreSQL names a set returning function's column after its alias, and on the pin the
    /// column still answers to `range` as well, until a subquery or a column list renames it.
    pub(crate) also: Option<String>,
    /// Whether this is a copy of a joined-on column that a `USING` or `NATURAL` join keeps only for
    /// its own table's name.
    ///
    /// `a JOIN b USING (k)` has one `k` in `SELECT *` and for a bare `k`, and on the pin `a.k` and
    /// `b.k` still read each side's own value and `b.*` still has `k` in it. So the copy stays where
    /// it was, reachable with its table in front, and is left out of everything that reads a bare
    /// name or a bare star.
    pub(crate) hidden: bool,
    /// Which joined-on column this is a copy of, or the column a bare name reads for, when it is
    /// either. See `Joined`.
    pub(crate) using: Option<Joined>,
}

/// The place a column has among the copies of one column a `USING` or `NATURAL` join joined on.
///
/// Each copy and the column a bare name reads for them carry the same group, which is the binding
/// the first left copy had when the join was bound. A bare star needs it: on the pin it walks the
/// copies in order and puts the column where the first one `EXCLUDE` does not name is, so `*
/// EXCLUDE (a.k)` over `a JOIN b USING (k)` has `k` where `b.k` was.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Joined {
    /// One side's own copy, which `a.k` reads.
    Copy(ColumnBinding),
    /// The column a `RIGHT` or `FULL` join puts in front of the left copy for a bare name to read.
    Merged(ColumnBinding),
}

impl Joined {
    /// The group the column is in.
    pub(crate) fn group(self) -> ColumnBinding {
        match self {
            Joined::Copy(group) | Joined::Merged(group) => group,
        }
    }
}

/// The `rowid` of a table in scope, which a name reaches and a star does not.
///
/// Kept beside the columns rather than among them, because everything that reads the columns of a
/// scope as the columns of a row, such as a star, a subquery's output or a view's fields, would
/// otherwise have to step over it. Only a table the catalog holds has one, and not one that has a
/// column of that name, which then wins. On the pin it still counts in an ambiguity: `SELECT rowid
/// FROM a JOIN b ON true` is refused when only `b` has a column called `rowid`.
#[derive(Debug, Clone)]
pub(crate) struct Rowid {
    /// The column a name resolves to, bound one past the table's last column.
    pub(crate) column: Visible,
    /// The scan, which only produces the row number once something reads it.
    pub(crate) scan: NodeRef,
    /// Where the table's columns start in the scope, which orders it among them in an error.
    pub(crate) at: usize,
}

/// The columns a name can resolve against.
#[derive(Debug, Clone, Default)]
pub(crate) struct Scope {
    pub(crate) columns: Vec<Visible>,
    pub(crate) rowids: Vec<Rowid>,
}

impl Scope {
    /// A scope with nothing in it, which is what `SELECT 1` binds against.
    pub(crate) fn empty() -> Self {
        Self::default()
    }

    /// Everything on the left followed by everything on the right, which is what a join sees.
    pub(crate) fn concat(mut self, other: Self) -> Self {
        let start = self.columns.len();
        self.columns.extend(other.columns);
        self.rowids
            .extend(other.rowids.into_iter().map(|rowid| Rowid { at: rowid.at + start, ..rowid }));
        self
    }

    /// Makes `rowid` reachable for the table whose columns start at `at`.
    pub(crate) fn add_rowid(&mut self, column: Visible, scan: NodeRef, at: usize) {
        self.rowids.push(Rowid { column, scan, at });
    }

    /// The scan whose row number `binding` is, when it is a `rowid`.
    pub(crate) fn rowid_scan(&self, binding: ColumnBinding) -> Option<NodeRef> {
        self.rowids.iter().find(|rowid| rowid.column.binding == binding).map(|rowid| rowid.scan)
    }

    pub(crate) fn push(&mut self, column: Visible) {
        self.columns.push(column);
    }

    pub(crate) fn len(&self) -> usize {
        self.columns.len()
    }

    /// The column `#index` stands for, or how many there are to count when there are fewer.
    ///
    /// The pin counts every column of every table in the `FROM` clause in order, so both copies of
    /// a column a `USING` join joined on count, and the one a `RIGHT` or `FULL` join puts in front
    /// for a bare name does not, since that one is not any table's.
    pub(crate) fn positional(&self, index: u32) -> std::result::Result<&Visible, usize> {
        let counted: Vec<&Visible> = self
            .columns
            .iter()
            .filter(|column| !matches!(column.using, Some(Joined::Merged(_))))
            .collect();
        let at = usize::try_from(index).unwrap_or(usize::MAX).saturating_sub(1);
        counted.get(at).copied().ok_or(counted.len())
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
    pub(crate) fn resolve(&self, compare: IdentifierCompare, parts: &[&str]) -> Result<&Visible> {
        if let Some(visible) = self.resolve_optional(compare, parts)? {
            return Ok(visible);
        }
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
        Err(self.not_found(compare, table, column))
    }

    /// Resolves a name when it is present, while still reporting ambiguity.
    pub(crate) fn resolve_optional(
        &self,
        compare: IdentifierCompare,
        parts: &[&str],
    ) -> Result<Option<&Visible>> {
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
        let mut matched: Vec<(usize, &Visible)> = self
            .columns
            .iter()
            .enumerate()
            .filter(|(_, held)| {
                compare.same(&held.name, column)
                    && (table.is_some() || !(held.qualified || held.hidden))
                    && table.is_none_or(|table| compare.same(&held.table, table))
            })
            .collect();
        matched.extend(
            self.rowids
                .iter()
                .filter(|rowid| {
                    compare.same(&rowid.column.name, column)
                        && table.is_none_or(|table| compare.same(&rowid.column.table, table))
                })
                .map(|rowid| (rowid.at, &rowid.column)),
        );
        matched.sort_by_key(|(at, _)| *at);
        let matched: Vec<&Visible> = matched.into_iter().map(|(_, held)| held).collect();
        let matched = if matched.is_empty() {
            self.columns
                .iter()
                .filter(|held| {
                    held.also.as_deref().is_some_and(|also| compare.same(also, column))
                        && (table.is_some() || !held.hidden)
                        && table.is_none_or(|table| compare.same(&held.table, table))
                })
                .collect()
        } else {
            matched
        };
        match matched.as_slice() {
            [one] => Ok(Some(one)),
            [] => Ok(None),
            many => {
                let candidates: Vec<String> =
                    many.iter().map(|held| format!("{}.{}", held.table, held.name)).collect();
                // The column name is in double quotes and the candidates under it are in single
                // ones, which reads like a mistake and is what the pin prints:
                // `Ambiguous reference to column name "a" (use: 't.a' or 'u.a')`.
                let written = match table {
                    Some(table) => format!("{table}.{column}"),
                    None => column.to_string(),
                };
                Err(Error::binder(format!(
                    "Ambiguous reference to column name \"{column}\" (use: '{}')",
                    candidates.join("' or '")
                ))
                .state(SqlState::AMBIGUOUS_COLUMN)
                .pg(format!("column reference \"{written}\" is ambiguous")))
            }
        }
    }

    /// The columns a star expands to.
    ///
    /// # Errors
    ///
    /// If the qualifier names no table in scope, or if there is nothing in scope at all, which is
    /// `SELECT *` with no `FROM` clause and is an error rather than zero columns.
    pub(crate) fn star(
        &self,
        compare: IdentifierCompare,
        qualifier: Option<&str>,
    ) -> Result<Vec<&Visible>> {
        let matched: Vec<&Visible> = match qualifier {
            None => self.columns.iter().filter(|held| !held.hidden).collect(),
            Some(table) => {
                self.columns.iter().filter(|held| compare.same(&held.table, table)).collect()
            }
        };
        if matched.is_empty() {
            return Err(match qualifier {
                Some(table) => {
                    Error::binder(format!("Referenced table \"{table}\" not found in FROM clause!"))
                        .state(SqlState::UNDEFINED_TABLE)
                        .pg(format!("missing FROM-clause entry for table \"{table}\""))
                }
                None => Error::binder("* is not allowed in a query without a FROM clause")
                    .state(SqlState::SYNTAX_ERROR)
                    .pg("SELECT * with no tables specified is not valid"),
            });
        }
        Ok(matched)
    }

    /// Renames every column's table, which is what an alias on a subquery or a table does.
    pub(crate) fn relabel(&mut self, table: &str) {
        for column in &mut self.columns {
            column.table = table.to_string();
        }
        for rowid in &mut self.rowids {
            rowid.column.table = table.to_string();
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
            ))
            .state(SqlState::INVALID_COLUMN_REFERENCE)
            .unplaced());
        }
        self.rename_prefix(names);
        Ok(())
    }

    /// Replaces the column names, ignoring every name past the last column.
    ///
    /// The column list a `WITH` definition is written with is the one list DuckDB does not report
    /// as too long. `WITH c(a, b, d, e) AS (SELECT 1, 2) SELECT * FROM c` answers two columns named
    /// `a` and `b` on the pinned build, where the same list on a table alias is refused and where
    /// PostgreSQL refuses both. That is reproduced rather than corrected, and it is filed as
    /// tamnd/duckdb#8.
    pub(crate) fn rename_prefix(&mut self, names: &[&str]) {
        for (column, name) in self.columns.iter_mut().zip(names) {
            column.name = (*name).to_string();
            column.also = None;
        }
    }

    /// The visible columns as fields, which is what a view writes down for the catalog tables.
    ///
    /// The table name each one is reachable through is dropped, because a field is a name and a
    /// type and the catalog already knows which view it is looking at.
    pub(crate) fn fields(&self) -> Vec<Field> {
        self.columns
            .iter()
            .filter(|column| !column.hidden)
            .map(|column| Field::new(column.name.clone(), column.ty.clone()))
            .collect()
    }

    /// The table column of each column that `Self::fields` gives, where there is one.
    pub(crate) fn origins(&self) -> Vec<Option<Origin>> {
        self.columns.iter().filter(|column| !column.hidden).map(|column| column.origin).collect()
    }

    /// Drops everything from `position` on, which is what a semi or an anti join does to the right
    /// side once its condition has been bound.
    pub(crate) fn truncate(&mut self, position: usize) {
        self.columns.truncate(position);
        self.rowids.retain(|rowid| rowid.at < position);
    }

    /// Where a column of that name sits, if exactly one does.
    pub(crate) fn position_of(
        &self,
        compare: IdentifierCompare,
        table: Option<&str>,
        name: &str,
    ) -> Option<usize> {
        let mut found = None;
        for (at, held) in self.columns.iter().enumerate() {
            if compare.same(&held.name, name)
                && (table.is_some() || !held.hidden)
                && table.is_none_or(|table| compare.same(&held.table, table))
            {
                if found.is_some() {
                    return None;
                }
                found = Some(at);
            }
        }
        found
    }

    /// Whether anything in scope answers to this name, in any table.
    ///
    /// Not the same question as [`Scope::position_of`], which says no when two columns match. This
    /// one says yes, because the caller is `current_date` asking whether it is a column here at all
    /// and two columns called `current_date` is the ambiguity error rather than the session constant.
    /// That was measured: `SELECT current_date FROM t, u` with the name in both is
    /// `Ambiguous reference to column name "current_date"` on the pin.
    pub(crate) fn names(&self, compare: IdentifierCompare, column: &str) -> bool {
        self.columns.iter().any(|held| !held.hidden && compare.same(&held.name, column))
    }

    fn not_found(&self, compare: IdentifierCompare, table: Option<&str>, column: &str) -> Error {
        match table {
            Some(table) if self.columns.iter().all(|held| !compare.same(&held.table, table)) => {
                Error::binder(format!("Referenced table \"{table}\" not found in FROM clause!"))
                    .state(SqlState::UNDEFINED_TABLE)
                    .pg(format!("missing FROM-clause entry for table \"{table}\""))
            }
            // The pin says `Values list` for a table that is not one, the `excluded` row among
            // them, and that row is the one whose columns only a qualified name reaches.
            Some(table) => {
                let mut held = self.columns.iter().filter(|held| compare.same(&held.table, table));
                let kind = if held.all(|held| held.qualified) { "Values list" } else { "Table" };
                Error::binder(format!(
                    "{kind} \"{table}\" does not have a column named \"{column}\""
                ))
                .state(SqlState::UNDEFINED_COLUMN)
                .pg(format!("column {table}.{column} does not exist"))
            }
            None => Error::binder(format!(
                "Referenced column \"{column}\" not found in FROM clause!{}",
                self.candidates()
            ))
            .state(SqlState::UNDEFINED_COLUMN)
            .pg(format!("column \"{column}\" does not exist")),
        }
    }

    /// The `Candidate bindings:` part of a complaint about a name that is not here, empty when
    /// there is nothing in scope to suggest.
    ///
    /// On its own line in the binary and on the same line here, because an error is one line here
    /// and the sentence before it is the part anybody matches on.
    pub(crate) fn candidates(&self) -> String {
        let candidates: Vec<&str> = self
            .columns
            .iter()
            .filter(|held| !held.hidden)
            .map(|held| held.name.as_str())
            .collect();
        if candidates.is_empty() {
            String::new()
        } else {
            format!(" Candidate bindings: \"{}\"", candidates.join("\", \""))
        }
    }
}

/// The hint of PostgreSQL for a column name that is in no table here, as `errorMissingColumn` in
/// `parse_relation.c` makes it: the one or two columns whose names are nearest to the name.
///
/// `scopes` has the scope of the name first and then each outer scope, innermost first, which is
/// the order that PostgreSQL searches its range tables in. The distance of a column is the
/// Levenshtein distance of its name, plus the distance of its table name from `table` when the
/// name has a table. A column is a candidate when the distance of its name is at most half of the
/// length of the name and the full distance is at most 3. Two candidates at the best distance are
/// both named, and three or more are none. A column that the merge of a join made belongs to no
/// table and is not searched, because PostgreSQL searches no join. When a column has the exact
/// name in a table of the exact name, the reference fails for a different reason and this gives
/// no hint.
pub(crate) fn nearest<'a>(
    scopes: impl IntoIterator<Item = &'a Scope>,
    table: Option<&str>,
    column: &str,
) -> Option<String> {
    const MOST: usize = 3;
    let mut best = MOST + 1;
    let mut first: Option<&Visible> = None;
    let mut second: Option<&Visible> = None;
    let mut exact = false;
    let held = scopes.into_iter().flat_map(|scope| scope.columns.iter());
    for held in held.filter(|held| !held.table.is_empty()) {
        if matches!(held.using, Some(Joined::Merged(_))) {
            continue;
        }
        let penalty = table.map_or(0, |table| distance(table, &held.table));
        exact |= penalty == 0 && held.name == column;
        if penalty > best {
            continue;
        }
        let near = distance(&held.name, column);
        if near > column.len() / 2 {
            continue;
        }
        let near = near + penalty;
        if near < best {
            best = near;
            first = Some(held);
            second = None;
        } else if near == best {
            if second.is_some() {
                first = None;
                second = None;
            } else if first.is_some() {
                second = Some(held);
            }
        }
    }
    if exact {
        return None;
    }
    match (first, second) {
        (Some(first), None) => Some(format!(
            "Perhaps you meant to reference the column \"{}.{}\".",
            first.table, first.name
        )),
        (Some(first), Some(second)) => Some(format!(
            "Perhaps you meant to reference the column \"{}.{}\" or the column \"{}.{}\".",
            first.table, first.name, second.table, second.name
        )),
        _ => None,
    }
}

/// The fewest inserts, deletes and substitutions of characters that turn one name into the other.
fn distance(a: &str, b: &str) -> usize {
    let b: Vec<char> = b.chars().collect();
    let mut previous: Vec<usize> = (0..=b.len()).collect();
    let mut current = vec![0; b.len() + 1];
    for (i, left) in a.chars().enumerate() {
        current[0] = i + 1;
        for (j, &right) in b.iter().enumerate() {
            let substitution = previous[j] + usize::from(left != right);
            current[j + 1] = substitution.min(previous[j + 1] + 1).min(current[j] + 1);
        }
        std::mem::swap(&mut previous, &mut current);
    }
    previous[b.len()]
}

#[cfg(test)]
mod tests {
    use super::*;

    const DUCKDB: IdentifierCompare = IdentifierCompare::CaseInsensitive;

    fn scope() -> Scope {
        let mut scope = Scope::empty();
        scope.push(Visible {
            table: "hits".into(),
            name: "UserID".into(),
            binding: ColumnBinding::new(0, 0),
            ty: LogicalType::BigInt,
            not_null: false,
            key: None,
            default: None,
            origin: None,
            qualified: false,
            also: None,
            hidden: false,
            using: None,
        });
        scope.push(Visible {
            table: "hits".into(),
            name: "url".into(),
            binding: ColumnBinding::new(0, 1),
            ty: LogicalType::Varchar,
            not_null: false,
            key: None,
            default: None,
            origin: None,
            qualified: false,
            also: None,
            hidden: false,
            using: None,
        });
        scope.push(Visible {
            table: "visits".into(),
            name: "url".into(),
            binding: ColumnBinding::new(1, 0),
            ty: LogicalType::Varchar,
            not_null: false,
            key: None,
            default: None,
            origin: None,
            qualified: false,
            also: None,
            hidden: false,
            using: None,
        });
        scope
    }

    #[test]
    fn a_unique_name_resolves_without_a_table() {
        let scope = scope();
        let found = scope.resolve(DUCKDB, &["userid"]).expect("one column is called that");
        assert_eq!(found.binding, ColumnBinding::new(0, 0));
    }

    #[test]
    fn a_name_in_two_tables_needs_the_table() {
        let scope = scope();
        let error = scope.resolve(DUCKDB, &["url"]).expect_err("two columns are called url");
        assert!(error.message().contains("Ambiguous"), "{error}");
        let found = scope.resolve(DUCKDB, &["visits", "url"]).expect("qualified");
        assert_eq!(found.binding, ColumnBinding::new(1, 0));
    }

    #[test]
    fn a_name_that_is_not_there_lists_what_is() {
        let error = scope().resolve(DUCKDB, &["nope"]).expect_err("no such column");
        assert!(error.message().contains("not found in FROM clause"), "{error}");
        assert!(error.message().contains("UserID"), "the message should say what is there");
    }

    #[test]
    fn a_table_that_is_not_there_says_that_rather_than_naming_the_column() {
        let error = scope().resolve(DUCKDB, &["nope", "url"]).expect_err("no such table");
        assert!(error.message().contains("Referenced table \"nope\""), "{error}");
    }

    #[test]
    fn a_star_expands_in_order_and_a_qualified_one_expands_to_its_table() {
        let scope = scope();
        let all = scope.star(DUCKDB, None).expect("three columns");
        assert_eq!(all.len(), 3);
        assert_eq!(all[0].name, "UserID");
        let one = scope.star(DUCKDB, Some("VISITS")).expect("one column, case insensitively");
        assert_eq!(one.len(), 1);
        assert_eq!(one[0].binding, ColumnBinding::new(1, 0));
    }

    #[test]
    fn a_qualified_name_ignores_the_schema_in_front_of_it() {
        let scope = scope();
        let found =
            scope.resolve(DUCKDB, &["memory", "main", "hits", "UserID"]).expect("four parts");
        assert_eq!(found.binding, ColumnBinding::new(0, 0));
    }

    #[test]
    fn a_postgresql_session_compares_the_bytes_of_a_name() {
        let scope = scope();
        let exact = IdentifierCompare::Exact;
        let error = scope.resolve(exact, &["userid"]).expect_err("no column is called userid");
        assert!(error.message().contains("not found in FROM clause"), "{error}");
        let found = scope.resolve(exact, &["UserID"]).expect("one column is called UserID");
        assert_eq!(found.binding, ColumnBinding::new(0, 0));
        scope.star(exact, Some("VISITS")).expect_err("no table is called VISITS");
        assert_eq!(scope.position_of(exact, None, "userid"), None);
        assert!(!scope.names(exact, "userid"));
    }
}
