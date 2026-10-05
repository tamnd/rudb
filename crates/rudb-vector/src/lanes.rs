//! Packed codes compared with a range eight at a time, without unpacking them first.
//!
//! A packed block of 64 codes of `width` bits is `width` words, and every eight codes of it are
//! exactly `width` bytes, so the eight codes of a group start on a byte. Within a group, code `i`
//! starts at bit `i * width`, which is a byte and a shift that depend only on the width and on `i`.
//! For a width of 25 or less a code and its shift fit in four bytes, so one shuffle puts each of
//! the eight codes' bytes into a 32 bit lane, one variable shift and one mask leave the code, and
//! one subtract, one unsigned minimum and one compare answer whether it is in the range. A
//! `movemask` makes that eight bits of the answer.
//!
//! The shuffle only moves bytes within each 128 bit half, so the low half is loaded from the
//! group's first byte and the high half from the byte code 4 starts in. Both loads are sixteen
//! bytes and both are inside the group's bytes and the sixteen after it, which the caller checks.
//!
//! What it replaces is [`crate::vector`]'s unpack into 64 words on the stack, a compare of each
//! word into a flag byte and the flags folded into a word, which on TPC-H q06 was a third of the
//! query.

/// Widest code the lanes take. A code starts up to seven bits into its first byte, and four bytes
/// hold 32 bits, so 25 is the most that is always inside them.
pub(crate) const LANE_WIDTH_MAX: usize = 25;

/// The bytes a block of 64 codes of `width` bits needs to be readable after its first byte: its own
/// `8 * width` and the sixteen the last group's high half reads past where it starts.
pub(crate) const fn readable(width: usize) -> usize {
    8 * width + 16
}

/// For each width, the shuffle that puts code `i`'s four bytes in lane `i`, and the shift that
/// leaves the code at the bottom of the lane.
const LANES: [([u8; 32], [u32; 8]); LANE_WIDTH_MAX + 1] = lanes();

const fn lanes() -> [([u8; 32], [u32; 8]); LANE_WIDTH_MAX + 1] {
    let mut table = [([0x80_u8; 32], [0_u32; 8]); LANE_WIDTH_MAX + 1];
    let mut width = 1;
    while width <= LANE_WIDTH_MAX {
        let half = 4 * width / 8;
        let mut lane = 0;
        while lane < 8 {
            let bit = lane * width;
            let first = if lane < 4 { 0 } else { half };
            let byte = bit / 8 - first;
            let mut k = 0;
            while k < 4 {
                #[expect(clippy::cast_possible_truncation, reason = "a byte under sixteen")]
                {
                    table[width].0[lane * 4 + k] = (byte + k) as u8;
                }
                k += 1;
            }
            #[expect(clippy::cast_possible_truncation, reason = "a shift under eight")]
            {
                table[width].1[lane] = (bit % 8) as u32;
            }
            lane += 1;
        }
        width += 1;
    }
    table
}

/// The bytes of `words`, in memory order. On x86-64 that is little end first, which is the order
/// the packed form numbers its bits in, so bit `b` of the codes is bit `b % 8` of byte `b / 8`.
#[cfg(all(target_arch = "x86_64", target_feature = "avx2"))]
#[allow(unsafe_code)]
pub(crate) fn bytes_of(words: &[u64]) -> &[u8] {
    // SAFETY: a `u8` has no alignment requirement and every bit pattern is one, and the slice
    // covers exactly the bytes of `words` for as long as `words` is borrowed.
    unsafe { std::slice::from_raw_parts(words.as_ptr().cast::<u8>(), size_of_val(words)) }
}

/// Bit `i` set when code `i` of the 64 at `bytes` is between `low` and `low + span`.
///
/// `bytes` starts at the block's first byte and holds at least [`readable`] bytes, `width` is
/// between one and [`LANE_WIDTH_MAX`], and `low` and `span` are at most the largest code.
#[cfg(all(target_arch = "x86_64", target_feature = "avx2"))]
#[inline]
#[allow(unsafe_code)]
pub(crate) fn within(bytes: &[u8], width: usize, low: u32, span: u32) -> u64 {
    use std::arch::x86_64::{
        _mm_loadu_si128, _mm256_and_si256, _mm256_castsi256_ps, _mm256_cmpeq_epi32,
        _mm256_loadu_si256, _mm256_min_epu32, _mm256_movemask_ps, _mm256_set_m128i,
        _mm256_set1_epi32, _mm256_shuffle_epi8, _mm256_srlv_epi32, _mm256_sub_epi32,
    };
    assert!((1..=LANE_WIDTH_MAX).contains(&width) && bytes.len() >= readable(width));
    let (shuffle, shifts) = &LANES[width];
    let half = 4 * width / 8;
    let mut word = 0_u64;
    // SAFETY: the build enables AVX2, which the `cfg` on this function checks. The table loads read
    // the 32 bytes of one entry. Group `g` loads sixteen bytes at `g * width` and at
    // `g * width + half`, and for the last group the second ends at `7 * width + half + 16`, which
    // is under `readable(width)`, so the assert above keeps every load inside `bytes`. `loadu` has
    // no alignment requirement.
    unsafe {
        let shuffle = _mm256_loadu_si256(shuffle.as_ptr().cast());
        let shifts = _mm256_loadu_si256(shifts.as_ptr().cast());
        #[expect(clippy::cast_possible_wrap, reason = "the lanes are read unsigned")]
        let (mask, low, span) = (
            _mm256_set1_epi32(((1_u32 << width) - 1) as i32),
            _mm256_set1_epi32(low as i32),
            _mm256_set1_epi32(span as i32),
        );
        let at = bytes.as_ptr();
        for group in 0..8 {
            let first = at.add(group * width);
            let lanes = _mm256_set_m128i(
                _mm_loadu_si128(first.add(half).cast()),
                _mm_loadu_si128(first.cast()),
            );
            let codes = _mm256_and_si256(
                _mm256_srlv_epi32(_mm256_shuffle_epi8(lanes, shuffle), shifts),
                mask,
            );
            let offset = _mm256_sub_epi32(codes, low);
            let kept = _mm256_cmpeq_epi32(_mm256_min_epu32(offset, span), offset);
            #[expect(clippy::cast_sign_loss, reason = "eight bits of a movemask")]
            let bits = _mm256_movemask_ps(_mm256_castsi256_ps(kept)) as u64;
            word |= bits << (group * 8);
        }
    }
    word
}

/// The 64 codes at `bytes`, each widened to a word, into `out`.
///
/// The same shuffle, shift and mask as [`within`], with each group's eight lanes widened to two
/// stores of four words rather than compared. An aggregate reads every code of a packed column it
/// sums, and unpacking a code at a time in scalar registers was what that cost, see
/// `spec/perf/108-codes-unpacked-in-lanes.md`. `bytes` is as [`within`] takes it.
#[cfg(all(target_arch = "x86_64", target_feature = "avx2"))]
#[inline]
#[allow(unsafe_code)]
pub(crate) fn unpack(bytes: &[u8], width: usize, out: &mut [u64; 64]) {
    use std::arch::x86_64::{
        _mm_loadu_si128, _mm256_and_si256, _mm256_castsi256_si128, _mm256_cvtepu32_epi64,
        _mm256_extracti128_si256, _mm256_loadu_si256, _mm256_set_m128i, _mm256_set1_epi32,
        _mm256_shuffle_epi8, _mm256_srlv_epi32, _mm256_storeu_si256,
    };
    assert!((1..=LANE_WIDTH_MAX).contains(&width) && bytes.len() >= readable(width));
    let (shuffle, shifts) = &LANES[width];
    let half = 4 * width / 8;
    // SAFETY: the loads are the ones [`within`] makes, which the assert keeps inside `bytes`. Group
    // `g` stores eight words at `8 * g`, so the last store ends at word 64, the end of `out`.
    // Neither `loadu` nor `storeu` has an alignment requirement.
    unsafe {
        let shuffle = _mm256_loadu_si256(shuffle.as_ptr().cast());
        let shifts = _mm256_loadu_si256(shifts.as_ptr().cast());
        #[expect(clippy::cast_possible_wrap, reason = "the lanes are read unsigned")]
        let mask = _mm256_set1_epi32(((1_u32 << width) - 1) as i32);
        let at = bytes.as_ptr();
        let to = out.as_mut_ptr();
        for group in 0..8 {
            let first = at.add(group * width);
            let lanes = _mm256_set_m128i(
                _mm_loadu_si128(first.add(half).cast()),
                _mm_loadu_si128(first.cast()),
            );
            let codes = _mm256_and_si256(
                _mm256_srlv_epi32(_mm256_shuffle_epi8(lanes, shuffle), shifts),
                mask,
            );
            let low = _mm256_cvtepu32_epi64(_mm256_castsi256_si128(codes));
            let high = _mm256_cvtepu32_epi64(_mm256_extracti128_si256::<1>(codes));
            _mm256_storeu_si256(to.add(group * 8).cast(), low);
            _mm256_storeu_si256(to.add(group * 8 + 4).cast(), high);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The shuffle and shift for a width reproduce a code read a bit at a time, for every width the
    /// lanes take, so the table is right whatever the hardware the tests run on.
    #[test]
    fn the_table_reads_every_code_of_every_width() {
        for (width, (shuffle, shifts)) in LANES.iter().enumerate().skip(1) {
            let codes: Vec<u64> = (0..64_u64).map(|i| (i * 2_654_435_761) % (1 << width)).collect();
            let mut bytes = vec![0_u8; readable(width)];
            for (i, &code) in codes.iter().enumerate() {
                for b in 0..width {
                    let bit = i * width + b;
                    bytes[bit / 8] |= u8::from(code >> b & 1 == 1) << (bit % 8);
                }
            }
            let half = 4 * width / 8;
            for (i, &code) in codes.iter().enumerate() {
                let (group, lane) = (i / 8, i % 8);
                let from = group * width + if lane < 4 { 0 } else { half };
                let four: Vec<u8> =
                    (0..4).map(|k| bytes[from + usize::from(shuffle[lane * 4 + k])]).collect();
                let read = u32::from_le_bytes(four.try_into().expect("four bytes")) >> shifts[lane];
                assert_eq!(u64::from(read) & ((1 << width) - 1), code, "width {width} code {i}");
            }
        }
    }

    #[cfg(all(target_arch = "x86_64", target_feature = "avx2"))]
    #[test]
    fn codes_unpacked_in_lanes_are_the_codes_packed() {
        for (width, (shuffle, shifts)) in LANES.iter().enumerate().skip(1) {
            let top = (1_u64 << width) - 1;
            let codes: Vec<u64> = (0..64_u64).map(|i| (i * 2_654_435_761) & top).collect();
            let mut bytes = vec![0xff_u8; readable(width)];
            bytes[..8 * width].fill(0);
            for (i, &code) in codes.iter().enumerate() {
                for b in 0..width {
                    let bit = i * width + b;
                    bytes[bit / 8] |= u8::from(code >> b & 1 == 1) << (bit % 8);
                }
            }
            let mut out = [u64::MAX; 64];
            unpack(&bytes, width, &mut out);
            assert_eq!(out.as_slice(), codes.as_slice(), "width {width}");
        }
    }

    #[cfg(all(target_arch = "x86_64", target_feature = "avx2"))]
    #[test]
    fn eight_lanes_agree_with_a_code_at_a_time() {
        for (width, (shuffle, shifts)) in LANES.iter().enumerate().skip(1) {
            let top = (1_u64 << width) - 1;
            let codes: Vec<u64> = (0..64_u64).map(|i| (i * 2_654_435_761) & top).collect();
            let mut bytes = vec![0_u8; readable(width)];
            for (i, &code) in codes.iter().enumerate() {
                for b in 0..width {
                    let bit = i * width + b;
                    bytes[bit / 8] |= u8::from(code >> b & 1 == 1) << (bit % 8);
                }
            }
            for (low, span) in [(0, top), (0, 0), (top, 0), (top / 3, top / 2), (1, top - 1)] {
                let expected = codes.iter().enumerate().fold(0, |word, (i, &code)| {
                    word | u64::from(code.wrapping_sub(low) <= span) << i
                });
                #[expect(clippy::cast_possible_truncation, reason = "under 2^25")]
                let got = within(&bytes, width, low as u32, span as u32);
                assert_eq!(got, expected, "width {width} low {low} span {span}");
            }
        }
    }
}
