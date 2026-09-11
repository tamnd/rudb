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
/// A dictionary keeps its nulls in the vector it points at and its own validity is always all
/// valid at construction, so reading `Vector::validity` on a dictionary reports every row valid and
/// a null then comes out as whatever value sits at code zero. Every kernel that takes a dictionary
/// path has to come through here instead. `Vector::flatten` and `Vector::dictionary_parts` both
/// carry the same warning, because this is a wrong answer that needs a filter, a null and one
/// specific form to reproduce and is correspondingly hard to find later.
pub(crate) fn nulls_of(vector: &Vector) -> Validity {
    let Some((codes, values)) = vector.dictionary_parts() else {
        return vector.validity().clone();
    };
    match values.validity() {
        Validity::AllValid => Validity::AllValid,
        Validity::AllInvalid if codes.is_empty() => Validity::AllValid,
        Validity::AllInvalid => Validity::AllInvalid,
        inner => Validity::from_iter(codes.len(), |index| inner.is_valid(codes[index] as usize)),
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
