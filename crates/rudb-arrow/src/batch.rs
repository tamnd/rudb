//! A chunk of columns, in Arrow's layout.
//!
//! Arrow calls this a record batch: a schema, a row count, and one array per field. It is the same
//! shape as a [`Chunk`](rudb_vector::Chunk), which is not a coincidence. Both are columnar, both
//! carry the whole batch's length once rather than per column, and both are the unit a consumer
//! pulls. The difference is only the bytes inside, which is what [`Array`] converts.

use rudb_common::{Error, Result};
use rudb_vector::Chunk;

use crate::array::Array;
use crate::types::{Field, Schema};

/// A batch of rows, as Arrow columns.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RecordBatch {
    schema: Schema,
    len: usize,
    columns: Vec<Array>,
}

impl RecordBatch {
    /// The batch a chunk becomes, with these column names.
    ///
    /// The names come from the caller because a chunk does not have any. A chunk is a run of
    /// vectors and the names live on whatever produced it, which for a query result is the plan's
    /// output list and for a table scan is the catalog.
    ///
    /// # Errors
    ///
    /// When the number of names is not the number of columns, and for anything [`Array::of`]
    /// rejects.
    pub fn of(chunk: &Chunk, names: &[String]) -> Result<Self> {
        if names.len() != chunk.width() {
            return Err(Error::internal(format!(
                "{} names for a chunk of {} columns",
                names.len(),
                chunk.width()
            )));
        }
        let mut columns = Vec::with_capacity(chunk.width());
        let mut fields = Vec::with_capacity(chunk.width());
        for (index, name) in names.iter().enumerate() {
            let array = Array::of(chunk.column(index)?)?;
            fields.push(Field::new(name.clone(), array.data_type().clone()));
            columns.push(array);
        }
        Ok(Self { schema: Schema::new(fields), len: chunk.len(), columns })
    }

    /// An empty batch of this schema.
    ///
    /// A result with no rows still has columns, and a consumer that reads the schema off the first
    /// batch needs one to read it off. Every array in it has length zero, which is a validity
    /// bitmap of nothing and a values buffer of nothing.
    ///
    /// # Errors
    ///
    /// Never, today. It returns a result so that a schema carrying a type with no empty array later
    /// has somewhere to say so.
    pub fn empty(schema: Schema) -> Result<Self> {
        let columns =
            schema.fields.iter().map(|field| Array::empty(field.data_type.clone())).collect();
        Ok(Self { schema, len: 0, columns })
    }

    /// The columns and their names.
    #[must_use]
    pub fn schema(&self) -> &Schema {
        &self.schema
    }

    /// How many rows.
    #[must_use]
    pub fn len(&self) -> usize {
        self.len
    }

    /// Whether there are none.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.len == 0
    }

    /// How many columns.
    #[must_use]
    pub fn width(&self) -> usize {
        self.columns.len()
    }

    /// The columns, left to right.
    #[must_use]
    pub fn columns(&self) -> &[Array] {
        &self.columns
    }

    /// One column, by position.
    #[must_use]
    pub fn column(&self, index: usize) -> Option<&Array> {
        self.columns.get(index)
    }
}

#[cfg(test)]
mod tests {
    use rudb_common::{LogicalType, Value};
    use rudb_vector::{Chunk, Vector};

    use super::RecordBatch;
    use crate::types::{DataType, Field, Schema};

    fn chunk() -> Chunk {
        let ids = Vector::from_values(
            LogicalType::Integer,
            &[Value::Integer(1), Value::Integer(2), Value::Null],
        )
        .expect("the values are integers");
        let names = Vector::from_values(
            LogicalType::Varchar,
            &[
                Value::Varchar("ann".to_string()),
                Value::Varchar("bo".to_string()),
                Value::Varchar("cy".to_string()),
            ],
        )
        .expect("the values are strings");
        Chunk::new(vec![ids, names]).expect("both columns are three long")
    }

    fn names() -> Vec<String> {
        vec!["id".to_string(), "name".to_string()]
    }

    #[test]
    fn a_chunk_becomes_one_array_per_column_with_the_names_it_was_given() {
        let batch = RecordBatch::of(&chunk(), &names()).expect("both types map onto Arrow");
        assert_eq!(batch.len(), 3);
        assert_eq!(batch.width(), 2);
        assert_eq!(
            batch.schema().fields,
            vec![Field::new("id", DataType::Int32), Field::new("name", DataType::Utf8)]
        );
        assert_eq!(batch.column(0).expect("the first column").null_count(), 1);
        assert_eq!(batch.column(1).expect("the second column").values(), b"annbocy");
    }

    #[test]
    fn the_wrong_number_of_names_is_refused_rather_than_padded_out() {
        let error = RecordBatch::of(&chunk(), &["id".to_string()])
            .expect_err("one name for two columns is a caller mistake");
        assert!(error.to_string().contains("1 names for a chunk of 2 columns"), "{error}");
    }

    #[test]
    fn a_result_with_no_rows_still_has_its_columns_and_their_types() {
        let schema = Schema::new(vec![
            Field::new("id", DataType::Int32),
            Field::new("name", DataType::Utf8),
        ]);
        let batch = RecordBatch::empty(schema.clone()).expect("an empty batch is always buildable");
        assert!(batch.is_empty());
        assert_eq!(batch.width(), 2);
        assert_eq!(batch.schema(), &schema);
    }

    #[test]
    fn a_chunk_of_no_columns_is_a_batch_of_no_columns() {
        let batch = RecordBatch::of(&Chunk::empty(&[]), &[]).expect("nothing to convert");
        assert_eq!(batch.width(), 0);
        assert!(batch.is_empty());
    }
}
