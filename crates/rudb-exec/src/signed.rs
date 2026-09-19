//! Reading a signed integer column as a block rather than a row at a time.
//!
//! Asking a [`Vector`] for one row costs a match on the layout, a widen to 128 bits and, at the
//! caller, a checked narrowing back. On a column of a million rows that is paid a million times for
//! a layout that does not change between rows. Reading the whole run up front costs one call and
//! leaves the loop above it indexing a flat slice.
//!
//! This is the same lift #237 did for the group hash, #539 for the key comparison and #804 for the
//! `COUNT(DISTINCT BIGINT)` scatter. It lives here rather than in one of the aggregate modules
//! because three of them want it, and a second copy would be free to drift on which layouts it
//! recognises.

use rudb_common::{Error, Result};
use rudb_vector::Vector;

/// One signed column read into a flat run of 64 bit values, once per chunk rather than once per row.
///
/// What this replaces is a reader that picked the layout once and then still walked the column a
/// row at a time, which cost a match on the reader, a bounds check and a widen to 128 bits that the
/// caller narrowed straight back. Callgrind put that at 13% of ClickBench 9 and 11% of ClickBench 8.
///
/// `Vector::signed_block` copies a flat `BIGINT` run and sign extends a narrower one, both of which
/// the compiler widens into a handful of instructions per lane, and it walks a packed run or a
/// sequence with the arithmetic those need. The forms it will not hand over, a dictionary and a run
/// among them, are filled here a row at a time, so no caller has to know which kind of column it was
/// given.
///
/// Nulls are the same question asked once. A column with none in it costs the loop above nothing,
/// and a column with some is asked row by row the way it always was, because a null is read out of
/// the column itself and not out of the buffer, which holds a zero for it.
///
/// The buffer lives for as long as the instance holding it does, so a chunk after the first
/// allocates nothing for this.
#[derive(Debug, Default)]
pub(crate) struct SignedBlock {
    held: Vec<i64>,
    nulled: bool,
}

impl SignedBlock {
    /// Reads the first `rows` values of one column into the buffer, replacing what was there.
    ///
    /// # Errors
    ///
    /// A column that is not a signed integer in any form, which is a plan that should not have
    /// reached a typed aggregate at all, or a value too wide for 64 bits.
    pub(crate) fn read(&mut self, rows: usize, column: &Vector) -> Result<()> {
        self.nulled = !column.none_null();
        if column.signed_block(&mut self.held) {
            return Ok(());
        }
        self.held.clear();
        self.held.reserve(rows);
        for row in 0..rows {
            self.held.push(match column.signed_at(row) {
                Some(value) => i64::try_from(value)
                    .map_err(|_| Error::internal("a signed column is out of range"))?,
                None if column.is_null_at(row) => 0,
                None => {
                    return Err(Error::internal("a signed column has no signed representation"));
                }
            });
        }
        Ok(())
    }

    /// Whether the column read holds a null anywhere in it.
    pub(crate) fn nulled(&self) -> bool {
        self.nulled
    }

    /// The buffer cut to the chunk's length, so the loop over it checks no bounds.
    ///
    /// # Errors
    ///
    /// A buffer shorter than the chunk, which would be a vector whose length disagreed with the
    /// chunk's.
    pub(crate) fn cut(&self, rows: usize) -> Result<&[i64]> {
        self.held
            .get(..rows)
            .ok_or_else(|| Error::internal("a signed column was read short of the chunk"))
    }
}

#[cfg(test)]
mod tests {
    use rudb_common::{LogicalType, Value};
    use rudb_vector::Vector;

    use super::SignedBlock;

    /// The layout the vector hands over as a block and the layout it refuses have to come back the
    /// same, because the caller above this cannot tell them apart and indexes both the same way.
    #[test]
    fn a_block_reads_the_same_values_from_a_run_and_from_a_dictionary() {
        let held = [Value::Integer(7), Value::Integer(-3), Value::Integer(7)];
        let flat = Vector::from_values(LogicalType::Integer, &held).expect("a flat run");
        let mut block = SignedBlock::default();
        block.read(3, &flat).expect("a flat run is read as a block");
        assert_eq!(block.cut(3).expect("three rows"), [7, -3, 7]);
        assert!(!block.nulled(), "no nulls in it");

        let values = Vector::from_values(LogicalType::Integer, &held[..2]).expect("two values");
        let coded = Vector::dictionary(vec![0, 1, 0], values).expect("a dictionary");
        let mut other = SignedBlock::default();
        other.read(3, &coded).expect("a dictionary is read a row at a time");
        assert_eq!(other.cut(3).expect("three rows"), [7, -3, 7]);
        assert!(!other.nulled(), "no nulls in it either");
    }

    /// A null reads as a zero and the flag says so, which is the contract the loops above rely on
    /// when they ask the column itself whether the row was null.
    #[test]
    fn a_null_leaves_a_zero_behind_and_raises_the_flag() {
        let held = [Value::BigInt(5), Value::Null, Value::BigInt(9)];
        let column = Vector::from_values(LogicalType::BigInt, &held).expect("one null in it");
        let mut block = SignedBlock::default();
        block.read(3, &column).expect("a column with a null is read");
        assert_eq!(block.cut(3).expect("three rows"), [5, 0, 9]);
        assert!(block.nulled(), "the flag says the loop has to ask the column");
        assert!(block.cut(4).is_err(), "more rows than the chunk held");
    }

    /// A packed column that starts partway through its own words, which is what a chunk cut out of a
    /// stored page looks like and the one shape whose arithmetic is easy to get off by a row.
    #[test]
    fn a_block_agrees_with_the_vector_on_an_offset_packed_column() {
        let values: Vec<Value> =
            (0..256).map(|row| Value::Integer((row * 37 % 127) - 30)).collect();
        let flat = Vector::from_values(LogicalType::Integer, &values).expect("an integer vector");
        let packed = flat.bit_packed().expect("the vector packs");
        assert!(packed.packed_parts().is_some());
        let cut = packed.slice(3, 200).expect("an offset packed vector");
        let mut block = SignedBlock::default();
        block.read(cut.len(), &cut).expect("a packed column is read as a block");
        let held = block.cut(cut.len()).expect("every row of it");
        for (row, &value) in held.iter().enumerate() {
            assert_eq!(i128::from(value), cut.signed_at(row).expect("a signed value"));
        }
    }
}
