//! The gathered side of a join, held as columns so that a match is a position rather than a row.
//!
//! A join finds pairs and then has to say what the pairs are. What this replaces said it by building
//! a `Vec<Value>` per output row: one heap allocation holding one boxed value per column, assembled
//! by cloning the driving row and the gathered row into it, and transposed back into columns at the
//! end by [`rows::pack`](crate::rows::pack). On the shape #880 measured that was 825ns per output
//! row, against a `SELECT sum(i) FROM range(...)` producing the same number of rows in a twentieth
//! of the time, so the boxing was the whole of the gap and none of it was the join.
//!
//! What a pair actually is, once the lookup has found it, is two numbers: which driving row and
//! which gathered row. So the probe writes two lists of numbers and the answer is built by gathering
//! each output column at those positions, which is one typed loop per column over a run of `u32`
//! rather than one allocation per row. [`Vector::gather`] is that loop and it was already here.
//!
//! For the driving side the positions index the chunk in hand. For the gathered side they have to
//! index the whole of it at once, and the whole of it arrives as a list of chunks, so this lays
//! those chunks end to end into one vector per column. [`Assembly`] is what lays them: it appends a
//! run of data after another run of data, which is a `memcpy` for a fixed width column and one arena
//! growth for a string one.
//!
//! # The row that is not there
//!
//! A `LEFT` join pads a driving row that matched nothing with nulls, and a `SINGLE` join does the
//! same. That is a row of the gathered side that does not exist, and the obvious way to write it is
//! a branch in the gather saying this one is padding. There is no branch: [`Vector::gather`] answers
//! null for a position past the end of the vector, so [`PAD`] is a position past the end and the
//! padded rows go through the same loop as the matched ones.

use rudb_common::{Error, LogicalType, Result, Value};
use rudb_vector::{Assembly, Chunk, Vector};

/// The position that gathers as null, which is what an unmatched driving row is paired with.
///
/// Past the end of any side, because a side with this many rows in it is refused by [`Build::new`]
/// before it is built.
pub(crate) const PAD: u32 = u32::MAX;

/// The gathered side of a join, one vector per column.
#[derive(Debug, Default)]
pub(crate) struct Build {
    columns: Vec<Vector>,
    rows: usize,
}

impl Build {
    /// The chunks laid end to end, a column at a time.
    ///
    /// `types` rather than the chunks' own types because a side with no chunks in it still has
    /// columns, and a join against an empty side still has to produce the right number of null
    /// columns for the driving rows a `LEFT` join keeps.
    ///
    /// # Errors
    ///
    /// [`rudb_common::ErrorCode::OutOfRange`] when the side has more rows than a position can name,
    /// and whatever the assembly says when a column is of a type it has no layout for.
    pub(crate) fn new(types: &[LogicalType], chunks: &[Chunk]) -> Result<Self> {
        let rows: usize = chunks.iter().map(Chunk::len).sum();
        if rows >= PAD as usize {
            return Err(Error::out_of_range(format!(
                "a join cannot gather {rows} rows, which is more than a position can name"
            )));
        }
        let mut columns = Vec::with_capacity(types.len());
        let mut at: Vec<u32> = Vec::new();
        for (index, ty) in types.iter().enumerate() {
            let mut assembly = Assembly::new(ty.clone(), rows)?;
            let mut base: u32 = 0;
            for chunk in chunks {
                let len = u32::try_from(chunk.len()).unwrap_or(PAD);
                at.clear();
                at.extend(base..base + len);
                assembly.place(&at, chunk.column(index)?)?;
                base += len;
            }
            columns.push(assembly.finish()?);
        }
        Ok(Self { columns, rows })
    }

    /// How many rows are in there.
    pub(crate) fn rows(&self) -> usize {
        self.rows
    }

    /// How many bytes the columns are holding.
    pub(crate) fn footprint(&self) -> u64 {
        self.columns
            .iter()
            .map(|column| u64::try_from(column.footprint()).unwrap_or(u64::MAX))
            .sum()
    }

    /// Every column read at those positions, [`PAD`] reading as null.
    ///
    /// # Errors
    ///
    /// Whatever the gather says about a column of a type it has no layout for.
    pub(crate) fn gather(&self, at: &[u32]) -> Result<Vec<Vector>> {
        self.columns.iter().map(|column| column.gather(at)).collect()
    }

    /// The chunk those positions make, which is what a residual condition is evaluated over.
    ///
    /// # Errors
    ///
    /// The same as [`Build::gather`].
    pub(crate) fn chunk(&self, at: &[u32]) -> Result<Chunk> {
        Chunk::with_rows(self.gather(at)?, at.len())
    }

    /// One row as values, for the sink that still pairs rows up one at a time.
    ///
    /// The row major path out of #880 rather than the columnar one. [`Build::gather`] is what the
    /// stream uses and this is what the `RIGHT` and `FULL` kinds use until they are moved over too.
    pub(crate) fn row(&self, at: u32) -> Vec<Value> {
        self.columns.iter().map(|column| column.value_at(at as usize)).collect()
    }
}

#[cfg(test)]
mod tests {
    use rudb_common::{LogicalType, Value};
    use rudb_vector::{Chunk, Data, Vector};

    use super::{Build, PAD};

    fn chunk(values: &[i32], text: &[&str]) -> Chunk {
        let numbers = Vector::flat(LogicalType::Integer, Data::Int32(values.to_vec().into()))
            .expect("integers are an i32 layout");
        let strings = Vector::from_values(
            LogicalType::Varchar,
            &text.iter().map(|&word| Value::Varchar(word.to_string())).collect::<Vec<_>>(),
        )
        .expect("strings are a varlen layout");
        Chunk::new(vec![numbers, strings]).expect("two columns of the same length")
    }

    fn types() -> Vec<LogicalType> {
        vec![LogicalType::Integer, LogicalType::Varchar]
    }

    #[test]
    fn chunks_laid_end_to_end_read_back_in_the_order_they_were_given() {
        let side = Build::new(&types(), &[chunk(&[1, 2], &["a", "b"]), chunk(&[3], &["c"])])
            .expect("two chunks of two columns");
        assert_eq!(side.rows(), 3);
        let gathered = side.gather(&[0, 1, 2]).expect("three positions in range");
        assert_eq!(gathered[0].value_at(0), Value::Integer(1));
        assert_eq!(gathered[0].value_at(2), Value::Integer(3));
        assert_eq!(gathered[1].value_at(1), Value::Varchar("b".to_string()));
        assert_eq!(gathered[1].value_at(2), Value::Varchar("c".to_string()));
    }

    #[test]
    fn a_position_may_be_asked_for_more_than_once_and_in_any_order() {
        let side = Build::new(&types(), &[chunk(&[10, 20], &["x", "y"])])
            .expect("one chunk of two columns");
        let gathered = side.gather(&[1, 1, 0]).expect("three positions in range");
        assert_eq!(gathered[0].value_at(0), Value::Integer(20));
        assert_eq!(gathered[0].value_at(1), Value::Integer(20));
        assert_eq!(gathered[0].value_at(2), Value::Integer(10));
    }

    #[test]
    fn the_padding_position_reads_as_null_in_every_column() {
        let side = Build::new(&types(), &[chunk(&[7], &["z"])]).expect("one chunk of two columns");
        let gathered = side.gather(&[PAD, 0]).expect("a padded position and a real one");
        assert_eq!(gathered[0].value_at(0), Value::Null);
        assert_eq!(gathered[1].value_at(0), Value::Null);
        assert_eq!(gathered[0].value_at(1), Value::Integer(7));
    }

    #[test]
    fn a_side_with_no_chunks_still_has_its_columns_and_every_one_of_them_is_null() {
        let side = Build::new(&types(), &[]).expect("no chunks at all");
        assert_eq!(side.rows(), 0);
        let gathered = side.gather(&[PAD, PAD]).expect("two padded positions");
        assert_eq!(gathered.len(), 2);
        assert_eq!(gathered[0].value_at(0), Value::Null);
        assert_eq!(gathered[1].value_at(1), Value::Null);
    }

    #[test]
    fn a_row_read_as_values_is_the_row_that_went_in() {
        let side = Build::new(&types(), &[chunk(&[4, 5], &["p", "q"])]).expect("one chunk");
        assert_eq!(side.row(1), vec![Value::Integer(5), Value::Varchar("q".to_string())]);
    }
}
