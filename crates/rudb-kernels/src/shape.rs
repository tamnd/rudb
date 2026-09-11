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
/// A dictionary has two places to keep a null and `Vector::value_at` reads both, so this reads both
/// too. A kernel that read only `Vector::validity` would report every row of a dictionary valid
/// wherever the nulls are in the values, and a null would come out as whatever sits at code zero.
/// A kernel that read only the values would miss the other kind. Every kernel that takes a
/// dictionary path has to come through here. `Vector::flatten` and `Vector::dictionary_parts` both
/// carry the same warning, because this is a wrong answer that needs a filter, a null and one
/// specific form to reproduce and is correspondingly hard to find later.
///
/// Both kinds are real. A dictionary built from a filtered column points at values that already
/// carry the nulls, which is the common one. A dictionary out of the Parquet reader is the other:
/// a page holds only its non-null values, so the codes are dense and which rows are there is the
/// definition levels, which land in the mask at this level. Reading only the values was correct
/// until a Parquet file could reach a kernel, and then `s IS NULL` over a dictionary encoded column
/// with 586 nulls in it answered zero.
pub(crate) fn nulls_of(vector: &Vector) -> Validity {
    let Some((codes, values)) = vector.dictionary_parts() else {
        return vector.validity().clone();
    };
    let inside = match values.validity() {
        Validity::AllValid => Validity::AllValid,
        Validity::AllInvalid if codes.is_empty() => Validity::AllValid,
        Validity::AllInvalid => Validity::AllInvalid,
        inner => {
            // A gather by code, so the read side cannot go a word at a time, but the write side
            // can and does. `from_run` packs sixty four answers into one word instead of doing a
            // read modify write on the bitmap for every row.
            let live: Vec<bool> = codes.iter().map(|&code| inner.is_valid(code as usize)).collect();
            Validity::from_run(&live)
        }
    };
    match vector.validity() {
        // The common case, and it is worth keeping because `and` of an all valid side still walks
        // the other one to normalize it, on every vector, in every kernel.
        Validity::AllValid => inside,
        outer => outer.and(&inside, vector.len()),
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
    use super::*;

    /// Which rows a validity says are there, which is what is being asserted every time below.
    fn live(validity: &Validity, len: usize) -> Vec<bool> {
        (0..len).map(|row| validity.is_valid(row)).collect()
    }

    /// The three distinct values and one dictionary over them, in the order the tests use.
    fn values() -> Vector {
        Vector::from_values(
            LogicalType::Varchar,
            &[Value::Varchar("a".into()), Value::Varchar("b".into()), Value::Null],
        )
        .expect("builds")
    }

    #[test]
    fn a_vector_that_is_not_a_dictionary_is_its_own_validity() {
        let flat = Vector::from_values(
            LogicalType::Integer,
            &[Value::Integer(1), Value::Null, Value::Integer(3)],
        )
        .expect("builds");
        assert_eq!(live(&nulls_of(&flat), 3), [true, false, true]);
    }

    #[test]
    fn a_dictionary_whose_nulls_are_in_its_values_reads_them_through_the_codes() {
        let dictionary = Vector::dictionary(vec![0, 2, 1, 2], values()).expect("builds");
        assert_eq!(live(&nulls_of(&dictionary), 4), [true, false, true, false]);
    }

    #[test]
    fn a_dictionary_whose_nulls_are_at_its_own_level_reads_them_there() {
        // What the Parquet reader builds: the codes are dense because a page holds only its
        // non-null values, and the definition levels land in the mask at this level. Reading only
        // the values here answered that none of these rows was null.
        let plain = Vector::from_values(
            LogicalType::Varchar,
            &[Value::Varchar("a".into()), Value::Varchar("b".into())],
        )
        .expect("builds");
        let dictionary = Vector::dictionary(vec![0, 1, 0, 1], plain)
            .expect("builds")
            .with_validity(Validity::from_run(&[true, false, false, true]));
        assert_eq!(live(&nulls_of(&dictionary), 4), [true, false, false, true]);
    }

    #[test]
    fn a_dictionary_with_a_null_in_both_places_is_null_wherever_either_one_says_so() {
        let dictionary = Vector::dictionary(vec![0, 2, 1, 1], values())
            .expect("builds")
            .with_validity(Validity::from_run(&[true, true, false, true]));
        assert_eq!(live(&nulls_of(&dictionary), 4), [true, false, false, true]);
    }

    #[test]
    fn an_all_invalid_dictionary_is_all_invalid_however_valid_its_values_are() {
        let plain = Vector::from_values(LogicalType::Varchar, &[Value::Varchar("a".into())])
            .expect("builds");
        let dictionary = Vector::dictionary(vec![0, 0], plain)
            .expect("builds")
            .with_validity(Validity::AllInvalid);
        assert_eq!(live(&nulls_of(&dictionary), 2), [false, false]);
    }
}
