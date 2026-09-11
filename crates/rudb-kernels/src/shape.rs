//! The small amount of form handling that every specialized kernel repeats.
//!
//! Two things live here rather than in each kernel file. The first is reading a vector's nulls
//! correctly, which is not the same as reading its validity and is the single easiest way to
//! produce a wrong answer in this crate. The second is the row that a given form reads for a given
//! output row, which is the whole difference between the flat loop, the constant loop and the
//! dictionary loop and is worth writing down once.
//!
//! There is deliberately no `Side` struct with a branch in its `at` method. A branch per row on
//! which form this is would put back exactly the cost the specialization exists to remove, so the
//! index mapping is three separate functions and a kernel passes whichever one its form pair calls
//! for into a generic loop. That is one monomorphization per form pair and none of them contains
//! the branch.

use rudb_common::{LogicalType, Value};
use rudb_vector::{Validity, Vector};

/// Which rows of a vector are not null, as a kernel needs to read it.
///
/// A dictionary can keep its nulls in either of two places and a kernel has to read both. Reading
/// `Vector::validity` alone reports every row of an engine built dictionary valid, and a null then
/// comes out as whatever value sits at code zero. Every kernel that takes a dictionary path has to
/// come through here instead. `Vector::flatten` and `Vector::dictionary_parts` both carry the same
/// warning, because this is a wrong answer that needs a filter, a null and one specific form to
/// reproduce and is correspondingly hard to find later.
///
/// The two places are not a redundancy. A dictionary built by `Vector::dictionary` from a column
/// that already had nulls keeps them in the values it points at, because that is where they were.
/// A dictionary read out of a Parquet page cannot: a Parquet dictionary page holds no nulls at all
/// and the nulls are the definition levels, which are per row and so belong on the vector itself.
/// Both forms are legal, `Vector::value_at` has always read both, and a kernel that reads one of
/// them counts a real ClickBench column wrong. The `count(s)` over the committed fixture in
/// `crates/rudb/src/tests.rs` is the case that found it: 4096 instead of 3510.
pub(crate) fn nulls_of(vector: &Vector) -> Validity {
    let Some((codes, values)) = vector.dictionary_parts() else {
        return vector.validity().clone();
    };
    let outer = vector.validity();
    match values.validity() {
        Validity::AllValid => outer.clone(),
        Validity::AllInvalid if codes.is_empty() => Validity::AllValid,
        Validity::AllInvalid => Validity::AllInvalid,
        inner => {
            // A gather by code, so the read side cannot go a word at a time, but the write side
            // can and does. `from_run` packs sixty four answers into one word instead of doing a
            // read modify write on the bitmap for every row.
            let live: Vec<bool> = codes
                .iter()
                .enumerate()
                .map(|(row, &code)| outer.is_valid(row) && inner.is_valid(code as usize))
                .collect();
            Validity::from_run(&live)
        }
    }
}

/// Row `index` of a side that is stored one value per row.
pub(crate) fn identity(index: usize) -> usize {
    index
}

/// Row `index` of a side that is one value however long the vector is.
pub(crate) fn first(_: usize) -> usize {
    0
}

/// A one row vector holding `value`, so that a constant can be read through the same slice the
/// flat path reads.
///
/// This is what lets the constant loop be the flat loop with a different index mapping rather than
/// a second body. It allocates once per call to a kernel, not once per row, and for a varchar
/// literal that allocation is the only one the whole vector pays.
pub(crate) fn single(ty: &LogicalType, value: &Value) -> Option<Vector> {
    Vector::from_values(ty.clone(), std::slice::from_ref(value)).ok()
}

#[cfg(test)]
mod tests {
    use rudb_common::LogicalType;
    use rudb_vector::{Data, Validity, Vector};

    use super::nulls_of;

    /// A dictionary of two values and four rows, with the nulls wherever the caller puts them.
    fn dictionary(values: Validity, rows: Validity) -> Vector {
        let values = Vector::flat(LogicalType::BigInt, Data::Int64(vec![10, 20].into()))
            .expect("two values")
            .with_validity(values);
        Vector::dictionary(vec![0, 1, 0, 1], values).expect("a dictionary").with_validity(rows)
    }

    #[test]
    fn a_dictionary_read_from_a_page_keeps_its_nulls_on_the_vector() {
        // What every dictionary encoded Parquet column looks like. The dictionary page holds no
        // nulls at all, because the format does not put them there, and the definition levels are
        // per row. Reading the values' validity alone reports four rows and the answer is two.
        let vector =
            dictionary(Validity::AllValid, Validity::from_run(&[true, false, true, false]));
        assert_eq!(nulls_of(&vector).count_valid(4), 2);
    }

    #[test]
    fn a_dictionary_built_from_a_column_keeps_its_nulls_in_the_values() {
        // The other way round, which is what `Vector::dictionary` produces when the column it
        // compressed already had nulls in it.
        let vector = dictionary(Validity::from_run(&[true, false]), Validity::AllValid);
        assert_eq!(nulls_of(&vector).count_valid(4), 2);
    }

    #[test]
    fn nulls_in_both_places_are_both_of_them_nulls() {
        // Rows 1 and 3 are null because the value they point at is, and row 2 is null because the
        // row is. Only row 0 is left.
        let vector = dictionary(
            Validity::from_run(&[true, false]),
            Validity::from_run(&[true, true, false, true]),
        );
        assert_eq!(nulls_of(&vector).count_valid(4), 1);
    }
}
