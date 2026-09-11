//! A view: a name, the query it stands for, and the names its columns answer to.
//!
//! The body is text rather than anything bound. A view in DuckDB follows the tables underneath it,
//! which was measured: a view over `SELECT * FROM t` picks up a column that `ALTER TABLE t ADD
//! COLUMN` added after the view was created, and a view over a table that was then dropped is an
//! error when it is selected from rather than when the table went. Neither of those is possible if
//! what the catalog keeps is a plan, because a plan has the columns of the day it was built baked
//! into it. So the catalog keeps the query and the binder binds it again at every reference.

use crate::name::QualifiedName;

/// One view.
#[derive(Debug, Clone)]
pub struct View {
    name: QualifiedName,
    sql: String,
    aliases: Vec<String>,
}

impl View {
    /// A view over `sql`, whose columns answer to `aliases` as far as that list goes.
    #[must_use]
    pub fn new(name: QualifiedName, sql: String, aliases: Vec<String>) -> Self {
        Self { name, sql, aliases }
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
}
