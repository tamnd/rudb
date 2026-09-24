//! What one `ALTER TABLE` changes, once the binder has resolved every name in it.
//!
//! The pin rebuilds a table's entry for every alter and checks the new entry the way it checks a
//! new table, so an alter that would leave a constraint broken is refused and the table stays as it
//! was. The same happens here: the change is made to a copy of the table and the copy only goes in
//! once it has taken the rows. The rows are worked out by the binder's plan for the changes that
//! move data, which are adding a column, dropping one and changing a type, and the catalog only
//! checks them.

use rudb_common::{Field, LogicalType};

use crate::name::QualifiedName;

/// One change to a table, or to a view for a rename.
#[derive(Debug, Clone)]
pub enum Alteration {
    /// `RENAME TO`.
    Rename(String),
    /// `RENAME COLUMN`, with the `CHECK` constraints written again over the new name.
    RenameColumn {
        /// The column, by place.
        column: usize,
        /// Its new name.
        to: String,
        /// Every `CHECK` of the table as it reads after the rename.
        checks: Vec<String>,
    },
    /// `ADD COLUMN`, which goes on the end.
    AddColumn {
        /// The new column.
        field: Field,
        /// Its `DEFAULT` as SQL.
        default: Option<String>,
        /// The sequences that default calls `nextval` on.
        sequences: Vec<QualifiedName>,
    },
    /// `DROP COLUMN`, with the `CHECK` constraints that are left once the ones over only this
    /// column are gone.
    DropColumn {
        /// The column, by place.
        column: usize,
        /// Every `CHECK` the table keeps.
        checks: Vec<String>,
    },
    /// `SET DEFAULT` or `DROP DEFAULT`.
    Default {
        /// The column, by place.
        column: usize,
        /// The new default as SQL, or `None` to drop it.
        default: Option<String>,
        /// The sequences that default calls `nextval` on.
        sequences: Vec<QualifiedName>,
    },
    /// `SET NOT NULL` or `DROP NOT NULL`.
    NotNull {
        /// The column, by place.
        column: usize,
        /// Whether it is `SET`.
        set: bool,
    },
    /// `SET DATA TYPE`.
    Type {
        /// The column, by place.
        column: usize,
        /// The new type.
        ty: LogicalType,
    },
}

impl Alteration {
    /// Whether the pin lets this through on a table that another table's foreign key points at.
    /// Adding a column and changing a default leave every column the key could name where it was.
    #[must_use]
    pub fn keeps_dependents(&self) -> bool {
        matches!(self, Self::AddColumn { .. } | Self::Default { .. })
    }
}
