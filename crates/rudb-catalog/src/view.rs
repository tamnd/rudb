//! A view: a name, the query it stands for, and the names its columns answer to.
//!
//! The body is text rather than anything bound. A view in DuckDB follows the tables underneath it,
//! which was measured: a view over `SELECT * FROM t` picks up a column that `ALTER TABLE t ADD
//! COLUMN` added after the view was created, and a view over a table that was then dropped is an
//! error when it is selected from rather than when the table went. Neither of those is possible if
//! what the catalog keeps is a plan, because a plan has the columns of the day it was built baked
//! into it. So the catalog keeps the query and the binder binds it again at every reference.
//!
//! # Then what does a view say its columns are
//!
//! `duckdb_columns()` lists a view's columns and `duckdb_views()` reports how many there are, and
//! neither of those can bind anything. What upstream keeps is a cache: the columns the last bind
//! produced, written down on the entry and read back by both tables. It goes stale and it is meant
//! to, which was measured on the pin. Create a view over `SELECT * FROM t`, add a column to `t`, and
//! both tables still report the old two. Read the view once and both report three. The staleness is
//! not a bug to route around, it is the behaviour, and `is_bound` is the column that says whether
//! the cache holds anything at all.
//!
//! So the same thing is kept here. The binder already works these columns out twice over, once at
//! `CREATE VIEW` to check the alias list against them and once at every reference to bind the body,
//! and it threw the answer away both times. Now it writes it here. The lock is on the entry rather
//! than on the catalog, because refreshing happens while a query is being bound and the catalog is
//! held by a shared reference at that point.

use std::sync::{Arc, RwLock};

use rudb_common::Field;

use crate::catalog::DETACHED;
use crate::name::QualifiedName;

/// One view.
#[derive(Debug, Clone)]
pub struct View {
    name: QualifiedName,
    sql: String,
    aliases: Vec<String>,
    /// What `duckdb_views()` reports as `view_oid`, stamped by the catalog when this goes in.
    oid: i64,
    /// The columns the last bind of the body produced. See the module doc for why this is a cache.
    columns: Arc<RwLock<Vec<Field>>>,
}

impl View {
    /// A view over `sql`, whose columns answer to `aliases` as far as that list goes.
    ///
    /// `columns` is what binding the body at creation produced, after the alias list was applied.
    #[must_use]
    pub fn new(
        name: QualifiedName,
        sql: String,
        aliases: Vec<String>,
        columns: Vec<Field>,
    ) -> Self {
        Self { name, sql, aliases, oid: DETACHED, columns: Arc::new(RwLock::new(columns)) }
    }

    /// The number the catalog tables join on, and [`DETACHED`] for a view not in a catalog.
    #[must_use]
    pub fn oid(&self) -> i64 {
        self.oid
    }

    /// Stamps the oid, which only [`crate::Catalog::create_view`] does.
    pub(crate) fn stamp(&mut self, oid: i64) {
        self.oid = oid;
    }

    /// The three part name.
    #[must_use]
    pub fn name(&self) -> &QualifiedName {
        &self.name
    }

    /// The body, as the text that was written.
    #[must_use]
    pub fn sql(&self) -> &str {
        &self.sql
    }

    /// The column names the statement gave, which rename a prefix of what the body produces.
    ///
    /// Shorter than the body's output, or empty, is the ordinary case. `CREATE VIEW v (a) AS SELECT
    /// 1, 2` answers under `a` and `2`, which was measured, so a short list is a rename of the front
    /// of the list and not a projection down to it. Longer is refused by the binder before a view
    /// is ever made.
    #[must_use]
    pub fn aliases(&self) -> &[String] {
        &self.aliases
    }

    /// The columns the last bind of the body produced, which is what the catalog tables report.
    ///
    /// A copy rather than a borrow, because the list can change under a reader and holding the lock
    /// across a whole table scan would mean a query that reads `duckdb_columns()` blocking a query
    /// that reads a view. Both lists are a handful of fields long.
    #[must_use]
    pub fn columns(&self) -> Vec<Field> {
        self.columns.read().unwrap_or_else(|held| held.into_inner()).clone()
    }

    /// Writes down what binding the body just produced, which the binder does at every reference.
    pub fn remember(&self, columns: Vec<Field>) {
        *self.columns.write().unwrap_or_else(|held| held.into_inner()) = columns;
    }
}

#[cfg(test)]
mod tests {
    use rudb_common::{Field, LogicalType};

    use super::View;
    use crate::name::QualifiedName;

    fn view(columns: Vec<Field>) -> View {
        View::new(
            QualifiedName::new("memory", "main", "v"),
            "SELECT x FROM t".to_string(),
            Vec::new(),
            columns,
        )
    }

    #[test]
    fn a_view_reports_the_columns_it_was_made_with() {
        let made = view(vec![Field::new("x", LogicalType::Integer)]);
        assert_eq!(made.columns(), vec![Field::new("x", LogicalType::Integer)]);
    }

    /// The staleness the pin has, in the one form rudb can reach without an `ALTER TABLE`.
    #[test]
    fn a_bind_writes_over_what_the_last_one_left() {
        let made = view(vec![Field::new("x", LogicalType::Integer)]);
        made.remember(vec![
            Field::new("x", LogicalType::Integer),
            Field::new("z", LogicalType::Varchar),
        ]);
        assert_eq!(made.columns().len(), 2);
        assert_eq!(made.columns()[1].name, "z");
    }

    /// A clone of an entry is the same entry, so the two share the cache rather than drifting.
    #[test]
    fn a_copy_of_a_view_sees_what_the_original_was_told() {
        let made = view(Vec::new());
        let copy = made.clone();
        made.remember(vec![Field::new("x", LogicalType::Integer)]);
        assert_eq!(copy.columns().len(), 1);
    }
}
