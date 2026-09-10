//! Bit packing in the FastLanes unified transposed layout.
//!
//! Packing N values of a fixed bit width into a dense buffer is the bottom of every integer
//! encoding in `spec/06-compression.md` section 6.2. FOR subtracts a base and packs. DELTA
//! differences and packs. DICT produces codes and packs them. So this is the one kernel that runs
//! over more bytes than anything else in the system, and the layout it uses decides whether the
//! decoder can be data parallel or has to walk a dependency chain.
//!
//! The obvious layout writes value 0 in the low bits of the first word, value 1 above it, and so
//! on. Unpacking that requires knowing where the previous value ended, which is a sequential
//! dependency, and a SIMD implementation has to fight it with shuffles that differ per width and
//! per instruction set. FastLanes takes the other road. The 1024 values of a vector are seen as a
//! matrix of `T` rows by `1024 / T` lanes, where `T` is the bit width of the type, and the packing
//! runs down the rows of every lane at once. Every lane has the same bit schedule, so unpacking
//! lane 0 and lane 31 is the same instruction sequence with no cross lane data movement at all.
//! That is what makes one scalar reference implementation and one AVX-512 implementation and one
//! NEON implementation agree bit for bit, and it is why the vector size is 1024 rather than a
//! rounder number.
//!
//! The price is that the values come out permuted within the vector. Row `r` lane `l` is not the
//! `r * lanes + l`th value of the input. Section 6.2 says why that is acceptable: an operator
//! working inside one vector does not care what order the rows are in, so the permutation only
//! has to be undone when a vector is materialized in row order. [`transpose`] and [`untranspose`]
//! are that step, and they are deliberately separate from [`pack_transposed`] and
//! [`unpack_transposed`] so that the engine can keep data permuted through a whole pipeline and
//! pay for the reordering once at the end rather than twice per operator.
//!
//! The permutation itself is a fixed shuffle of the eight bit groups of a row index, in the order
//! 0, 4, 2, 6, 1, 5, 3, 7. That order is not arbitrary. It is the one that makes an eight way
//! interleave of the rows land back in sequence under the pairwise unpacking pattern the paper
//! uses, and the important property for us is only that it is a bijection that both directions
//! agree on.

use rudb_common::{Error, Result};

/// How many values a packed unit holds. One vector, per `spec/06-compression.md` section 6.2.
pub const VALUES: usize = 1024;

/// The interleaving order of the eight row groups. See the module documentation.
const ORDER: [usize; 8] = [0, 4, 2, 6, 1, 5, 3, 7];

mod sealed {
    pub trait Sealed {}
    impl Sealed for u8 {}
    impl Sealed for u16 {}
    impl Sealed for u32 {}
    impl Sealed for u64 {}
}

/// An unsigned integer type that can be bit packed.
///
/// Sealed, because the layout constants are only correct for the four widths that divide 1024 into
/// a whole number of lanes, and because every kernel here does its arithmetic in `u64` and relies
/// on every implementor fitting in one.
pub trait Packable: sealed::Sealed + Copy + Ord + std::fmt::Debug {
    /// Width of the type in bits. `T` in the module documentation.
    const WIDTH: usize;
    /// How many of these fit in the 1024 bit virtual register, which is how many lanes there are.
    const LANES: usize = VALUES / Self::WIDTH;

    /// Widens to the type the packing arithmetic is done in.
    fn to_u64(self) -> u64;
    /// Narrows back. The high bits are already known to be zero.
    fn from_u64(value: u64) -> Self;
}

macro_rules! impl_packable {
    ($($ty:ty),*) => {$(
        impl Packable for $ty {
            const WIDTH: usize = <$ty>::BITS as usize;

            #[inline]
            fn to_u64(self) -> u64 {
                u64::from(self)
            }

            #[inline]
            fn from_u64(value: u64) -> Self {
                value as $ty
            }
        }
    )*};
}

impl_packable!(u8, u16, u32, u64);

/// A mask of the low `bits` bits, correct at 0 and at 64 where the shift would overflow.
#[inline]
const fn low_mask(bits: usize) -> u64 {
    if bits >= 64 { u64::MAX } else { (1u64 << bits) - 1 }
}

/// A right shift that saturates to zero at 64 rather than overflowing.
#[inline]
const fn shift_right(value: u64, bits: usize) -> u64 {
    if bits >= 64 { 0 } else { value >> bits }
}

/// Where the value at row `row` lane `lane` of the transposed matrix came from in the input.
///
/// The row index is split into a group and an offset within the group, the group is permuted by
/// the fixed order in the module documentation, and the two are recombined with the offset as the
/// high part. The lane index is untouched, which is the property that makes the layout lane
/// parallel.
///
/// # Panics
///
/// If `row` is not below `T::WIDTH` or `lane` is not below `T::LANES`.
#[inline]
#[must_use]
pub fn source_index<T: Packable>(row: usize, lane: usize) -> usize {
    assert!(row < T::WIDTH, "row {row} is outside a {} bit type", T::WIDTH);
    assert!(lane < T::LANES, "lane {lane} is outside {} lanes", T::LANES);
    let group_size = T::WIDTH / 8;
    let group = row / group_size;
    let offset = row % group_size;
    ((offset * 8) + ORDER[group]) * T::LANES + lane
}

/// Rewrites 1024 values from row order into the transposed layout.
///
/// # Errors
///
/// If either slice is not exactly [`VALUES`] long.
pub fn transpose<T: Packable>(input: &[T], output: &mut [T]) -> Result<()> {
    check_vector_len(input.len(), "input")?;
    check_vector_len(output.len(), "output")?;
    for row in 0..T::WIDTH {
        for lane in 0..T::LANES {
            output[row * T::LANES + lane] = input[source_index::<T>(row, lane)];
        }
    }
    Ok(())
}

/// Rewrites 1024 values from the transposed layout back into row order.
///
/// # Errors
///
/// If either slice is not exactly [`VALUES`] long.
pub fn untranspose<T: Packable>(input: &[T], output: &mut [T]) -> Result<()> {
    check_vector_len(input.len(), "input")?;
    check_vector_len(output.len(), "output")?;
    for row in 0..T::WIDTH {
        for lane in 0..T::LANES {
            output[source_index::<T>(row, lane)] = input[row * T::LANES + lane];
        }
    }
    Ok(())
}

/// How many words of `T` a packed vector of the given width occupies.
///
/// Every lane contributes `width` words, which is the same `width * 1024` bits the naive layout
/// would use. The layout costs nothing in space.
#[must_use]
pub fn packed_len<T: Packable>(width: usize) -> usize {
    width * T::LANES
}

/// The smallest bit width that can hold every value in the slice. Zero for an empty slice or a
/// slice of zeros, which [`pack_transposed`] handles as the degenerate case that stores nothing.
#[must_use]
pub fn required_width<T: Packable>(values: &[T]) -> usize {
    let max = values.iter().copied().max().map_or(0, T::to_u64);
    (64 - max.leading_zeros()) as usize
}

/// Packs a transposed vector at a fixed bit width.
///
/// The input is 1024 values already in the layout [`transpose`] produces, and the output is
/// [`packed_len`] words. Every lane is packed independently and the loop over lanes is the one a
/// SIMD implementation replaces with a single register.
///
/// # Errors
///
/// If the input is not [`VALUES`] long, if the output is not [`packed_len`] long, if `width`
/// exceeds the width of the type, or if a value does not fit in `width` bits.
pub fn pack_transposed<T: Packable>(input: &[T], width: usize, output: &mut [T]) -> Result<()> {
    check_vector_len(input.len(), "input")?;
    check_width::<T>(width)?;
    if output.len() != packed_len::<T>(width) {
        return Err(Error::internal(format!(
            "a {width} bit packed vector is {} words, not {}",
            packed_len::<T>(width),
            output.len()
        )));
    }
    if width == 0 {
        // Nothing is stored. The caller has already established that every value is zero, either
        // by asking for `required_width` or by being the CONSTANT encoding, and the check below
        // enforces it rather than trusting it.
        return check_all_zero(input);
    }

    let mask = low_mask(width);
    let lanes = T::LANES;
    for lane in 0..lanes {
        // Bits already sitting in `accumulator`, always below `T::WIDTH` between iterations.
        let mut filled = 0usize;
        let mut accumulator = 0u64;
        let mut word = 0usize;
        for row in 0..T::WIDTH {
            let value = input[row * lanes + lane].to_u64();
            if value & !mask != 0 {
                return Err(Error::internal(format!("value {value} does not fit in {width} bits")));
            }
            accumulator |= value << filled;
            filled += width;
            if filled >= T::WIDTH {
                output[word * lanes + lane] = T::from_u64(accumulator & low_mask(T::WIDTH));
                word += 1;
                // The only value that can straddle the word boundary is the one just written, so
                // the carry is a shift of it rather than anything kept from earlier rows.
                let consumed = width - (filled - T::WIDTH);
                filled -= T::WIDTH;
                accumulator = shift_right(value, consumed);
            }
        }
        debug_assert_eq!(filled, 0, "a packed lane always ends on a word boundary");
    }
    Ok(())
}

/// Unpacks into the transposed layout. The inverse of [`pack_transposed`].
///
/// # Errors
///
/// If the input is not [`packed_len`] long, if the output is not [`VALUES`] long, or if `width`
/// exceeds the width of the type.
pub fn unpack_transposed<T: Packable>(input: &[T], width: usize, output: &mut [T]) -> Result<()> {
    check_width::<T>(width)?;
    check_vector_len(output.len(), "output")?;
    if input.len() != packed_len::<T>(width) {
        return Err(Error::internal(format!(
            "a {width} bit packed vector is {} words, not {}",
            packed_len::<T>(width),
            input.len()
        )));
    }
    if width == 0 {
        output.fill(T::from_u64(0));
        return Ok(());
    }

    let mask = low_mask(width);
    let lanes = T::LANES;
    for lane in 0..lanes {
        // Bits of the current word not yet handed out, right aligned in `buffer`.
        let mut available = 0usize;
        let mut buffer = 0u64;
        let mut word = 0usize;
        for row in 0..T::WIDTH {
            let value = if available >= width {
                let value = buffer & mask;
                buffer = shift_right(buffer, width);
                available -= width;
                value
            } else {
                let next = input[word * lanes + lane].to_u64();
                word += 1;
                let taken = width - available;
                let value = buffer | ((next & low_mask(taken)) << available);
                buffer = shift_right(next, taken);
                available = T::WIDTH - taken;
                value
            };
            output[row * lanes + lane] = T::from_u64(value);
        }
    }
    Ok(())
}

/// Packs a vector given in row order, transposing it first.
///
/// The engine does not use this. Data written by the storage layer is transposed once on the way
/// in and stays that way, per the module documentation. This exists for tests, for the format lab,
/// and for the one place that has to hand back a vector in the order the user gave it.
///
/// # Errors
///
/// As [`pack_transposed`].
pub fn pack<T: Packable>(input: &[T], width: usize, output: &mut [T]) -> Result<()> {
    check_vector_len(input.len(), "input")?;
    let mut transposed = vec![T::from_u64(0); VALUES];
    transpose(input, &mut transposed)?;
    pack_transposed(&transposed, width, output)
}

/// Unpacks into row order. The inverse of [`pack`], and see its note about who should call it.
///
/// # Errors
///
/// As [`unpack_transposed`].
pub fn unpack<T: Packable>(input: &[T], width: usize, output: &mut [T]) -> Result<()> {
    check_vector_len(output.len(), "output")?;
    let mut transposed = vec![T::from_u64(0); VALUES];
    unpack_transposed(input, width, &mut transposed)?;
    untranspose(&transposed, output)
}

fn check_vector_len(len: usize, what: &str) -> Result<()> {
    if len == VALUES {
        Ok(())
    } else {
        Err(Error::internal(format!("{what} is {len} values, and a packed unit is {VALUES}")))
    }
}

fn check_width<T: Packable>(width: usize) -> Result<()> {
    if width <= T::WIDTH {
        Ok(())
    } else {
        Err(Error::internal(format!("{width} bits does not fit in a {} bit type", T::WIDTH)))
    }
}

fn check_all_zero<T: Packable>(input: &[T]) -> Result<()> {
    match input.iter().position(|value| value.to_u64() != 0) {
        None => Ok(()),
        Some(index) => Err(Error::internal(format!(
            "a zero bit vector cannot hold {:?} at {index}",
            input[index]
        ))),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A xorshift, so that the test data is the same on every host and in every run without the
    /// workspace growing a dependency for it.
    struct Random(u64);

    impl Random {
        fn new() -> Self {
            Self(0x2545_f491_4f6c_dd1d)
        }

        fn next(&mut self) -> u64 {
            self.0 ^= self.0 << 13;
            self.0 ^= self.0 >> 7;
            self.0 ^= self.0 << 17;
            self.0
        }
    }

    fn sample<T: Packable>(width: usize) -> Vec<T> {
        let mut random = Random::new();
        (0..VALUES).map(|_| T::from_u64(random.next() & low_mask(width))).collect()
    }

    fn round_trip<T: Packable>(width: usize) {
        let values = sample::<T>(width);
        let mut packed = vec![T::from_u64(0); packed_len::<T>(width)];
        pack(&values, width, &mut packed).unwrap();
        let mut back = vec![T::from_u64(0); VALUES];
        unpack(&packed, width, &mut back).unwrap();
        assert_eq!(back, values, "{width} bits of a {} bit type", T::WIDTH);
    }

    #[test]
    fn every_width_of_every_type_round_trips() {
        for width in 0..=8 {
            round_trip::<u8>(width);
        }
        for width in 0..=16 {
            round_trip::<u16>(width);
        }
        for width in 0..=32 {
            round_trip::<u32>(width);
        }
        for width in 0..=64 {
            round_trip::<u64>(width);
        }
    }

    #[test]
    fn the_transposed_form_also_round_trips_without_being_reordered() {
        // What the engine actually does: transpose once, then pack and unpack any number of times
        // without ever going back to row order.
        let values = sample::<u32>(19);
        let mut transposed = vec![0u32; VALUES];
        transpose(&values, &mut transposed).unwrap();
        let mut packed = vec![0u32; packed_len::<u32>(19)];
        pack_transposed(&transposed, 19, &mut packed).unwrap();
        let mut back = vec![0u32; VALUES];
        unpack_transposed(&packed, 19, &mut back).unwrap();
        assert_eq!(back, transposed);
    }

    #[test]
    fn the_permutation_is_a_bijection() {
        // Every value has to land somewhere and no two may land in the same place, or a round trip
        // would silently drop rows. Checked for all four widths because the group size changes.
        fn check<T: Packable>() {
            let mut seen = vec![false; VALUES];
            for row in 0..T::WIDTH {
                for lane in 0..T::LANES {
                    let index = source_index::<T>(row, lane);
                    assert!(!seen[index], "{index} is written twice for {} bits", T::WIDTH);
                    seen[index] = true;
                }
            }
            assert!(seen.into_iter().all(|hit| hit));
        }
        check::<u8>();
        check::<u16>();
        check::<u32>();
        check::<u64>();
    }

    #[test]
    fn transposing_is_not_the_identity() {
        // If it were, the test above would be passing on a layout that is not the FastLanes one.
        let values: Vec<u32> = (0..VALUES).map(|index| index as u32).collect();
        let mut transposed = vec![0u32; VALUES];
        transpose(&values, &mut transposed).unwrap();
        assert_ne!(transposed, values);
        let mut back = vec![0u32; VALUES];
        untranspose(&transposed, &mut back).unwrap();
        assert_eq!(back, values);
    }

    #[test]
    fn a_full_width_pack_is_the_data_itself() {
        // 64 bits of a 64 bit type has no packing to do, and the loop that handles the general case
        // has to get the degenerate one right rather than shifting by 64 and wrapping.
        let values = sample::<u64>(64);
        let mut transposed = vec![0u64; VALUES];
        transpose(&values, &mut transposed).unwrap();
        let mut packed = vec![0u64; packed_len::<u64>(64)];
        pack_transposed(&transposed, 64, &mut packed).unwrap();
        assert_eq!(packed, transposed);
    }

    #[test]
    fn a_zero_width_vector_stores_nothing_and_reads_back_as_zeros() {
        let values = vec![0u32; VALUES];
        assert_eq!(required_width(&values), 0);
        let mut packed = Vec::new();
        pack(&values, 0, &mut packed).unwrap();
        let mut back = vec![7u32; VALUES];
        unpack(&packed, 0, &mut back).unwrap();
        assert_eq!(back, values);
    }

    #[test]
    fn required_width_is_the_bits_of_the_largest_value() {
        assert_eq!(required_width::<u32>(&[]), 0);
        assert_eq!(required_width::<u32>(&[0, 0]), 0);
        assert_eq!(required_width::<u32>(&[1]), 1);
        assert_eq!(required_width::<u32>(&[255, 3]), 8);
        assert_eq!(required_width::<u32>(&[256]), 9);
        assert_eq!(required_width::<u64>(&[u64::MAX]), 64);
    }

    #[test]
    fn a_value_too_wide_for_the_width_is_an_error_rather_than_silent_truncation() {
        let mut values = vec![0u32; VALUES];
        values[500] = 8;
        let mut transposed = vec![0u32; VALUES];
        transpose(&values, &mut transposed).unwrap();
        let mut packed = vec![0u32; packed_len::<u32>(3)];
        let error = pack_transposed(&transposed, 3, &mut packed).unwrap_err();
        assert!(error.message().contains("does not fit in 3 bits"), "{error}");
    }

    #[test]
    fn a_wrong_sized_buffer_is_an_error() {
        let values = vec![0u32; VALUES];
        let mut packed = vec![0u32; 3];
        let error = pack(&values, 5, &mut packed).unwrap_err();
        assert!(error.message().contains("words"), "{error}");

        let short = vec![0u32; 7];
        let mut output = vec![0u32; VALUES];
        let error = unpack(&short, 5, &mut output).unwrap_err();
        assert!(error.message().contains("words"), "{error}");
    }

    #[test]
    fn a_nonzero_value_at_zero_width_is_an_error() {
        let mut values = vec![0u32; VALUES];
        values[9] = 1;
        let mut packed = Vec::new();
        let error = pack(&values, 0, &mut packed).unwrap_err();
        assert!(error.message().contains("zero bit vector"), "{error}");
    }

    #[test]
    fn packing_at_a_width_the_type_cannot_hold_is_an_error() {
        let values = vec![0u16; VALUES];
        let mut packed = vec![0u16; 17 * 64];
        let error = pack(&values, 17, &mut packed).unwrap_err();
        assert!(error.message().contains("16 bit type"), "{error}");
    }

    #[test]
    fn the_packed_size_is_the_same_as_the_naive_layout() {
        for width in 0..=32 {
            assert_eq!(packed_len::<u32>(width) * 32, width * VALUES);
        }
    }
}
