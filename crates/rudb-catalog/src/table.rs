//! A table: a name, some columns, and the rows.

use rudb_common::{Error, Field, LogicalType, Result};
use rudb_storage::MemoryTable;

use crate::name::{QualifiedName, same_name};

/// One table.
///
/// The rows are a [`MemoryTable`] because that is what M0 has. When the storage format arrives the
/// field changes and this type does not, which is the reason the catalog holds the rows behind a
/// handle rather than being the rows.
#[derive(Debug, Clone)]
pub struct Table {
    name: QualifiedName,
    columns: Vec<Field>,
    rows: MemoryTable,
}

impl Table {
    /// A table with no rows in it.
    ///
    /// # Errors
    ///
    /// If two columns have the same name, which SQL does not allow and which would make a column
    /// reference ambiguous in a way no error message could explain later.
    pub fn new(name: QualifiedName, columns: Vec<Field>) -> Result<Self> {
        for (at, column) in columns.iter().enumerate() {
            if let Some(earlier) =
                columns[..at].iter().find(|held| same_name(&held.name, &column.name))
            {
                return Err(Error::binder(format!(
                    "table \"{}\" has a duplicate column name \"{}\"",
                    name.table, earlier.name
                )));
            }
        }
        let types = columns.iter().map(|column| column.ty.clone()).collect();
        Ok(Self { name, columns, rows: MemoryTable::new(types) })
    }

    /// The three part name.
    #[must_use]
    pub fn name(&self) -> &QualifiedName {
        &self.name
    }

    /// The columns, in order.
    #[must_use]
    pub fn columns(&self) -> &[Field] {
        &self.columns
    }

    /// The column types, in order.
    #[must_use]
    pub fn types(&self) -> Vec<LogicalType> {
        self.columns.iter().map(|column| column.ty.clone()).collect()
    }

    /// Where a column sits, by name, under the identifier rule.
    #[must_use]
    pub fn column_index(&self, name: &str) -> Option<usize> {
        self.columns.iter().position(|column| same_name(&column.name, name))
    }

    /// The rows.
    #[must_use]
    pub fn rows(&self) -> &MemoryTable {
        &self.rows
    }

    /// The rows, to add to.
    pub fn rows_mut(&mut self) -> &mut MemoryTable {
        &mut self.rows
    }
}

#[cfg(test)]
mod tests {
    use rudb_common::Value;

    use super::*;

    fn hits() -> Table {
        Table::new(
            QualifiedName::new("memory", "main", "hits"),
            vec![
                Field::new("UserID", LogicalType::BigInt),
                Field::new("SearchPhrase", LogicalType::Varchar),
            ],
        )
        .expect("two columns with different names")
    }

    #[test]
    fn a_column_is_found_however_it_is_spelled() {
        let table = hits();
        assert_eq!(table.column_index("userid"), Some(0));
        assert_eq!(table.column_index("SEARCHPHRASE"), Some(1));
        assert_eq!(table.column_index("nope"), None);
    }

    #[test]
    fn two_columns_with_one_name_is_caught() {
        let error = Table::new(
            QualifiedName::new("memory", "main", "t"),
            vec![Field::new("a", LogicalType::Integer), Field::new("A", LogicalType::Varchar)],
        )
        .expect_err("two columns called a");
        assert!(error.message().contains("duplicate column"), "{error}");
    }

    #[test]
    fn a_new_table_is_empty_and_typed() {
        let mut table = hits();
        assert!(table.rows().is_empty());
        assert_eq!(table.rows().types(), table.types());
        table
            .rows_mut()
            .append_rows(&[vec![Value::BigInt(1), Value::Varchar("a".to_string())]])
            .expect("a row of the table's own types");
        assert_eq!(table.rows().len(), 1);
    }
}
