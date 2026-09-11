//! A table: a name, some columns, and the rows.

use rudb_common::{Error, Field, LogicalType, Result, Value};
use rudb_storage::MemoryTable;
use rudb_vector::{Chunk, Form};

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
    ///
    /// This is the way past the constraint check, and the two `append` methods here are the way
    /// through it. A caller that already knows what it is holding, such as the loader that built
    /// the chunk out of a file the table was declared from, can take this one.
    pub fn rows_mut(&mut self) -> &mut MemoryTable {
        &mut self.rows
    }

    /// Adds a chunk, refusing a null in a column that said it would not have one.
    ///
    /// # Errors
    ///
    /// If the chunk does not match the table, or if a `NOT NULL` column is handed a null. DuckDB
    /// raises a constraint error there and so does this, with the same shape of message, because a
    /// program that catches one by its text is a program rudb has to not surprise.
    pub fn append(&mut self, chunk: Chunk) -> Result<()> {
        self.refuse_nulls(&chunk)?;
        self.rows.append(chunk)
    }

    /// Adds rows of single values, refusing a null in a column that said it would not have one.
    ///
    /// # Errors
    ///
    /// If a row is not as wide as the table, if a value will not convert to its column's type, or
    /// if a `NOT NULL` column is handed a null.
    pub fn append_rows(&mut self, rows: &[Vec<Value>]) -> Result<()> {
        for row in rows {
            for (at, column) in self.columns.iter().enumerate() {
                if column.not_null && row.get(at).is_some_and(Value::is_null) {
                    return Err(self.null_in(&column.name));
                }
            }
        }
        self.rows.append_rows(rows)
    }

    /// Checks a chunk against the `NOT NULL` columns before any of it is kept.
    ///
    /// A table with no such column pays one walk of the column list and touches no data, which is
    /// most tables. A column that does refuse nulls is checked through its validity mask when the
    /// mask is the whole story, which is one word per sixty four rows rather than a read per row.
    /// A dictionary or a constant can hold the null in the body it points at instead, where the
    /// mask cannot see it, so those two are asked value by value.
    fn refuse_nulls(&self, chunk: &Chunk) -> Result<()> {
        for (at, column) in self.columns.iter().enumerate() {
            if !column.not_null {
                continue;
            }
            let vector = chunk.column(at)?;
            let found = match vector.form() {
                Form::Flat | Form::Sequence => {
                    vector.validity().has_nulls(vector.len())
                        && (0..vector.len()).any(|row| !vector.validity().is_valid(row))
                }
                _ => (0..vector.len()).any(|row| vector.value_at(row).is_null()),
            };
            if found {
                return Err(self.null_in(&column.name));
            }
        }
        Ok(())
    }

    /// The error DuckDB raises when a null reaches a column that refuses them.
    fn null_in(&self, column: &str) -> Error {
        Error::constraint(format!("NOT NULL constraint failed: {}.{}", self.name.table, column))
    }
}

#[cfg(test)]
mod tests {
    use rudb_vector::Vector;

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

    /// A table whose first column refuses nulls and whose second does not.
    fn required() -> Table {
        Table::new(
            QualifiedName::new("memory", "main", "hits"),
            vec![
                Field::required("UserID", LogicalType::BigInt),
                Field::new("SearchPhrase", LogicalType::Varchar),
            ],
        )
        .expect("two columns with different names")
    }

    #[test]
    fn a_null_in_a_not_null_column_is_refused() {
        let mut table = required();
        let error = table
            .append_rows(&[vec![Value::Null, Value::Varchar("a".to_string())]])
            .expect_err("a null in UserID");
        assert_eq!(error.message(), "NOT NULL constraint failed: hits.UserID");
        assert!(table.rows().is_empty(), "the row was kept anyway");
    }

    #[test]
    fn a_null_in_a_column_that_allows_them_is_kept() {
        let mut table = required();
        table.append_rows(&[vec![Value::BigInt(7), Value::Null]]).expect("a null in SearchPhrase");
        assert_eq!(table.rows().len(), 1);
    }

    #[test]
    fn a_chunk_is_checked_through_its_mask() {
        let mut table = required();
        let phrase = Vector::constant(LogicalType::Varchar, Value::Varchar("a".to_string()), 2);
        let good = Chunk::new(vec![
            Vector::from_values(LogicalType::BigInt, &[Value::BigInt(1), Value::BigInt(2)])
                .expect("two ids"),
            phrase.clone(),
        ])
        .expect("two columns of two rows");
        table.append(good).expect("no nulls anywhere");
        let bad = Chunk::new(vec![
            Vector::from_values(LogicalType::BigInt, &[Value::BigInt(1), Value::Null])
                .expect("an id and a null"),
            phrase,
        ])
        .expect("two columns of two rows");
        let error = table.append(bad).expect_err("a null in UserID");
        assert_eq!(error.message(), "NOT NULL constraint failed: hits.UserID");
        assert_eq!(table.rows().len(), 2, "the bad chunk was kept anyway");
    }

    #[test]
    fn a_null_hiding_in_a_constant_is_found() {
        let mut table = required();
        let chunk = Chunk::new(vec![
            Vector::constant(LogicalType::BigInt, Value::Null, 4),
            Vector::constant(LogicalType::Varchar, Value::Varchar("a".to_string()), 4),
        ])
        .expect("two columns of four rows");
        let error = table.append(chunk).expect_err("a constant null in UserID");
        assert_eq!(error.message(), "NOT NULL constraint failed: hits.UserID");
    }
}
