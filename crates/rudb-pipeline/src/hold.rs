//! Sparse chunks gathered into full ones between the operators of a pipeline.
//!
//! A scan that keeps a few rows of every part, a join that matches a few rows of every chunk and a
//! filter that passes a few of them all hand on a chunk with most of its rows gone, and every
//! operator below pays what it pays per chunk for each of them: a dispatch, a buffer sized for a
//! whole chunk and zeroed, a gather set up for a handful of rows. TPC-H q9 is the shape. The link
//! keeps 319 thousand of lineitem's six million rows, which is about 436 out of every 8192, so the
//! two link joins, the hash join, the projection and the aggregate below the scan each run 733
//! times over chunks that are a twentieth full.
//!
//! So the driver holds a chunk with few rows at the boundary it reached rather than pushing it on,
//! and pushes the held rows on as one chunk once there are enough of them. That is a copy of each
//! held row, which is the trade `chunk.compaction` is about, and it is taken here because what it
//! saves is not one redirection per row but everything each later operator spends per chunk.
//!
//! Held rows are pushed on at the end of every morsel, before the next one starts, so a sink that
//! puts chunks back into the order the morsels were cut in still finds every row under the morsel
//! it came from, and the rows of one morsel stay in the order they were read.
//!
//! An operator that says [`Progress::Done`](crate::Progress::Done) wants no more rows, so anything
//! held above it or at it is dropped rather than pushed. What is held below it is still owed to
//! the operators there and goes on as usual.

use std::mem;

use rudb_common::Result;
use rudb_vector::{Chunk, VECTOR_SIZE, Vector, concat};

/// A chunk with fewer rows than this is held rather than pushed on.
const SPARSE: usize = VECTOR_SIZE / 8;

/// Held rows are pushed on once there are at least this many, which with every piece under
/// [`SPARSE`] keeps what goes on under a full chunk.
const FULL: usize = VECTOR_SIZE / 2;

/// The rows held at one boundary, in the order they arrived.
#[derive(Debug, Default)]
pub(crate) struct Held {
    pieces: Vec<Chunk>,
    rows: usize,
}

impl Held {
    /// Whether nothing is held here.
    pub(crate) fn is_empty(&self) -> bool {
        self.pieces.is_empty()
    }

    /// Whether `chunk` is sparse enough to hold.
    ///
    /// An empty one is not, since there is nothing to hold, and a marked chunk is not, because its rows are the whole columns with a selection beside them
    /// and only the consumer that asked for one reads it that way.
    pub(crate) fn wants(chunk: &Chunk) -> bool {
        chunk.kept().is_none() && !chunk.is_empty() && chunk.len() < SPARSE
    }

    /// Takes `chunk`, leaving an empty one in its place, and answers the rows to push on when
    /// there are enough of them.
    pub(crate) fn take(&mut self, chunk: &mut Chunk) -> Result<Option<Vec<Chunk>>> {
        let chunk = mem::replace(chunk, Chunk::empty(&[]));
        self.rows += chunk.len();
        self.pieces.push(chunk);
        if self.rows < FULL {
            return Ok(None);
        }
        self.out().map(Some)
    }

    /// Everything held, as few chunks as it can be laid into, and nothing held afterwards.
    pub(crate) fn out(&mut self) -> Result<Vec<Chunk>> {
        self.rows = 0;
        let pieces = mem::take(&mut self.pieces);
        if pieces.len() < 2 {
            return Ok(pieces);
        }
        match laid(&pieces)? {
            Some(chunk) => Ok(vec![chunk]),
            // A column with no flat layout, so the pieces go on as they came. Correct and no
            // faster, and nothing in TPC-H reaches it.
            None => Ok(pieces),
        }
    }
}

/// The pieces laid end to end, column by column, or `None` when a column cannot be.
fn laid(pieces: &[Chunk]) -> Result<Option<Chunk>> {
    let rows = pieces.iter().map(Chunk::len).sum();
    let types = pieces[0].types();
    if pieces.iter().any(|piece| piece.types() != types) {
        return Ok(None);
    }
    let mut columns = Vec::with_capacity(types.len());
    for (at, ty) in types.iter().enumerate() {
        let parts: Vec<&Vector> = pieces.iter().map(|piece| &piece.columns()[at]).collect();
        if let Some(column) = concat(ty, &parts)? {
            columns.push(column);
            continue;
        }
        // A scan hands up a null constant for a column only its filter read, and the same value
        // over more rows is still one value. Flattening it would write every row of it.
        if let Some(value) = parts[0].constant_value()
            && parts.iter().all(|part| part.constant_value() == Some(value))
        {
            columns.push(Vector::constant(ty.clone(), value.clone(), rows));
            continue;
        }
        // flatten: a selection over a stored column is a dictionary over it, which has no layout
        // that pieces share, and copying the kept rows out is the copy this is here to make.
        let flat = parts.iter().map(|part| part.flatten()).collect::<Result<Vec<_>>>()?;
        match concat(ty, &flat)? {
            Some(column) => columns.push(column),
            None => return Ok(None),
        }
    }
    Chunk::with_rows(columns, rows).map(Some)
}

#[cfg(test)]
mod tests {
    use rudb_common::{LogicalType, Value};
    use rudb_vector::{Chunk, Vector};

    use super::Held;

    fn coded(indices: Vec<u32>, values: &[i64]) -> Chunk {
        let values: Vec<Value> = values.iter().map(|value| Value::BigInt(*value)).collect();
        let dictionary = Vector::from_values(LogicalType::BigInt, &values).unwrap();
        Chunk::new(vec![Vector::dictionary(indices, dictionary).unwrap()]).unwrap()
    }

    /// Two selections over different stored columns share no layout, so they are flattened and
    /// laid end to end, in the order they arrived.
    #[test]
    fn pieces_with_nothing_in_common_are_flattened_into_one() {
        let mut held = Held::default();
        let mut first = coded(vec![1, 0], &[7, 8]);
        let mut second = coded(vec![2], &[4, 5, 6]);
        assert!(Held::wants(&first));
        assert!(held.take(&mut first).unwrap().is_none());
        assert!(first.is_empty());
        assert!(held.take(&mut second).unwrap().is_none());

        let out = held.out().unwrap();
        assert!(held.is_empty());
        assert_eq!(out.len(), 1);
        let values: Vec<Value> = out[0].columns()[0].iter().collect();
        assert_eq!(values, vec![Value::BigInt(8), Value::BigInt(7), Value::BigInt(6)]);
    }

    /// The same constant in every piece stays one constant over all of their rows.
    #[test]
    fn a_constant_in_every_piece_stays_a_constant() {
        let mut held = Held::default();
        for rows in [3, 5] {
            let null = Vector::constant(LogicalType::BigInt, Value::Null, rows);
            let mut chunk = Chunk::new(vec![null]).unwrap();
            assert!(held.take(&mut chunk).unwrap().is_none());
        }
        let out = held.out().unwrap();
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].len(), 8);
        assert_eq!(out[0].columns()[0].constant_value(), Some(&Value::Null));
    }

    #[test]
    fn an_empty_chunk_is_not_held() {
        assert!(!Held::wants(&Chunk::empty(&[LogicalType::BigInt])));
    }
}
