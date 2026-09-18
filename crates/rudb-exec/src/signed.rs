//! Reading a signed integer column where it lies rather than a row at a time.
//!
//! Asking a [`Vector`] for one row costs a match on the layout, a widen to 128 bits and, at the
//! caller, a checked narrowing back. On a column of a million rows that is paid a million times for
//! a layout that does not change between rows. Picking the layout once and then reading the run of
//! words, or the packed bits, where the values actually lie costs none of that.
//!
//! This is the same lift #237 did for the group hash, #539 for the key comparison and #804 for the
//! `COUNT(DISTINCT BIGINT)` scatter. It lives here rather than in one of the aggregate modules
//! because two of them want it and a third is likely to, and a second copy would be free to drift
//! on which layouts it recognises.

use rudb_vector::{Data, Packed, Vector};

/// One signed column, with the layout decided once instead of once a row.
///
/// [`Self::Other`] is the fallback for a layout with no run to read, and it is the row at a time
/// path the rest of this exists to avoid.
pub(crate) enum SignedReader<'a> {
    Int16(&'a [i16]),
    Int32(&'a [i32]),
    Int64(&'a [i64]),
    Packed(Packed<'a>),
    Other(&'a Vector),
}

impl<'a> SignedReader<'a> {
    /// The cheapest reader the vector's layout allows.
    pub(crate) fn new(vector: &'a Vector) -> Self {
        match vector.data() {
            Some(Data::Int16(values)) => Self::Int16(values.as_slice()),
            Some(Data::Int32(values)) => Self::Int32(values.as_slice()),
            Some(Data::Int64(values)) => Self::Int64(values.as_slice()),
            _ => match vector.packed_parts() {
                Some(packed) => Self::Packed(packed),
                None => Self::Other(vector),
            },
        }
    }

    /// The value at one row, widened.
    ///
    /// # Panics
    ///
    /// Panics on the fallback path when the vector has no signed representation for the row, which
    /// callers rule out by only reaching here for a column the binder typed as a signed integer.
    pub(crate) fn at(&self, row: usize) -> i128 {
        match self {
            Self::Int16(values) => i128::from(values[row]),
            Self::Int32(values) => i128::from(values[row]),
            Self::Int64(values) => i128::from(values[row]),
            Self::Packed(packed) => {
                let words = packed.words();
                let width = packed.width();
                let bit = (packed.offset() + row) * width as usize;
                let word = bit / u64::BITS as usize;
                let shift = (bit % u64::BITS as usize) as u32;
                let mask = u64::MAX >> (u64::BITS - width);
                let low = words.get(word).copied().unwrap_or(0) >> shift;
                let taken = u64::BITS - shift;
                let code = if taken >= width {
                    low & mask
                } else {
                    let high = words.get(word + 1).copied().unwrap_or(0) << taken;
                    (low | high) & mask
                };
                packed.base() + i128::from(code)
            }
            Self::Other(vector) => {
                vector.signed_at(row).expect("the typed aggregate input is a signed value")
            }
        }
    }
}
