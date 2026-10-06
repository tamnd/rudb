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
pub(crate) fn within(bytes: &[u8], width: usize, low: u32, span: u32) -> u64 {
    let mut word = u64::MAX;
    within_words(bytes, width, low, span, std::slice::from_mut(&mut word), true);
    word
}

/// [`within`] for each block of `words`, block `b` starting `b * width` words into `bytes`, and how
/// many rows are set after.
///
/// With `fresh` each word is set to what its block holds, and without it each word is narrowed to
/// that and a word already empty is not read at all, which is how a filter's second column skips
/// the blocks its first one emptied. One call for the blocks of a chunk rather than one a block,
/// because the tables, the mask and the range go into registers once here, and a call a block
/// put that setup and a function's way in and out around 64 rows. On TPC-H q06 that was as much
/// again as the compares themselves.
///
/// `bytes` holds the blocks and the sixteen bytes [`readable`] asks for after the last one.
#[cfg(all(target_arch = "x86_64", target_feature = "avx2"))]
#[inline]
#[allow(unsafe_code)]
pub(crate) fn within_words(
    bytes: &[u8],
    width: usize,
    low: u32,
    span: u32,
    words: &mut [u64],
    fresh: bool,
) -> usize {
    use std::arch::x86_64::{
        _mm_loadu_si128, _mm256_and_si256, _mm256_castsi256_ps, _mm256_cmpeq_epi32,
        _mm256_loadu_si256, _mm256_min_epu32, _mm256_movemask_ps, _mm256_set_m128i,
        _mm256_set1_epi32, _mm256_shuffle_epi8, _mm256_srlv_epi32, _mm256_sub_epi32,
    };
    assert!((1..=LANE_WIDTH_MAX).contains(&width));
    let Some(last) = words.len().checked_sub(1) else { return 0 };
    assert!(bytes.len() >= last * 8 * width + readable(width));
    let (shuffle, shifts) = &LANES[width];
    let half = 4 * width / 8;
    let mut kept = 0;
    // SAFETY: the build enables AVX2, which the `cfg` on this function checks. The table loads read
    // the 32 bytes of one entry. Block `b` starts at `8 * b * width`, and its group `g` loads
    // sixteen bytes at `g * width` and at `g * width + half` from there, so for the last group of
    // the last block the second load ends at `8 * last * width + 7 * width + half + 16`, which is
    // under the length the assert above checks. `loadu` has no alignment requirement.
    unsafe {
        let shuffle = _mm256_loadu_si256(shuffle.as_ptr().cast());
        let shifts = _mm256_loadu_si256(shifts.as_ptr().cast());
        #[expect(clippy::cast_possible_wrap, reason = "the lanes are read unsigned")]
        let (mask, low, span) = (
            _mm256_set1_epi32(((1_u32 << width) - 1) as i32),
            _mm256_set1_epi32(low as i32),
            _mm256_set1_epi32(span as i32),
        );
        for (block, word) in words.iter_mut().enumerate() {
            if !fresh && *word == 0 {
                continue;
            }
            let at = bytes.as_ptr().add(8 * block * width);
            let mut found = 0_u64;
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
                let inside = _mm256_cmpeq_epi32(_mm256_min_epu32(offset, span), offset);
                #[expect(clippy::cast_sign_loss, reason = "eight bits of a movemask")]
                let bits = _mm256_movemask_ps(_mm256_castsi256_ps(inside)) as u64;
                found |= bits << (group * 8);
            }
            *word = if fresh { found } else { *word & found };
            kept += word.count_ones() as usize;
        }
    }
    kept
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

/// The codes of one group of eight at `first`, each in a 32 bit lane, for a side whose table,
/// shift, mask and half are the ones [`within_words`] sets up for its width.
#[cfg(all(target_arch = "x86_64", target_feature = "avx2"))]
#[inline(always)]
#[allow(unsafe_code)]
unsafe fn group_codes(
    first: *const u8,
    half: usize,
    shuffle: std::arch::x86_64::__m256i,
    shifts: std::arch::x86_64::__m256i,
    mask: std::arch::x86_64::__m256i,
) -> std::arch::x86_64::__m256i {
    use std::arch::x86_64::{
        _mm_loadu_si128, _mm256_and_si256, _mm256_set_m128i, _mm256_shuffle_epi8, _mm256_srlv_epi32,
    };
    // SAFETY: the caller has the sixteen bytes at `first` and at `first + half` to read, and
    // `loadu` has no alignment requirement.
    unsafe {
        let lanes = _mm256_set_m128i(
            _mm_loadu_si128(first.add(half).cast()),
            _mm_loadu_si128(first.cast()),
        );
        _mm256_and_si256(_mm256_srlv_epi32(_mm256_shuffle_epi8(lanes, shuffle), shifts), mask)
    }
}

/// Each word of `words` set or narrowed to the rows of its block where the code of `left` stands
/// to the code of `right` plus `shift` as the three flags say, which is how two packed columns of
/// one chunk are compared with each other without either being unpacked.
///
/// The test is `right + shift > left` with `SWAP`, `left > right + shift` without it, `==` in place
/// of `>` with `EQUAL`, and the answer turned round with `NOT`, which between them make all six
/// orders. A code is below 2^25 and the caller keeps `shift` below 2^30 either way, so both sides
/// fit a signed lane and one signed compare answers eight rows. `fresh` is as [`within_words`]
/// takes it, and each side is a `(bytes, width)` pair with the bytes it asks for there.
#[cfg(all(target_arch = "x86_64", target_feature = "avx2"))]
#[inline(always)]
#[allow(unsafe_code)]
pub(crate) fn against_words<const SWAP: bool, const EQUAL: bool, const NOT: bool>(
    left: (&[u8], usize),
    right: (&[u8], usize),
    shift: i32,
    words: &mut [u64],
    fresh: bool,
) {
    use std::arch::x86_64::{
        _mm256_add_epi32, _mm256_castsi256_ps, _mm256_cmpeq_epi32, _mm256_cmpgt_epi32,
        _mm256_loadu_si256, _mm256_movemask_ps, _mm256_set1_epi32,
    };
    let ((left, one), (right, other)) = (left, right);
    assert!((1..=LANE_WIDTH_MAX).contains(&one) && (1..=LANE_WIDTH_MAX).contains(&other));
    let Some(last) = words.len().checked_sub(1) else { return };
    assert!(left.len() >= last * 8 * one + readable(one));
    assert!(right.len() >= last * 8 * other + readable(other));
    let (left_shuffle, left_shifts) = &LANES[one];
    let (right_shuffle, right_shifts) = &LANES[other];
    let (left_half, right_half) = (4 * one / 8, 4 * other / 8);
    // SAFETY: the build enables AVX2, which the `cfg` on this function checks. Each side's loads
    // are the ones [`within_words`] makes over its own bytes and width, which the asserts above
    // keep inside them for the same reason they do there.
    unsafe {
        let left_shuffle = _mm256_loadu_si256(left_shuffle.as_ptr().cast());
        let left_shifts = _mm256_loadu_si256(left_shifts.as_ptr().cast());
        let right_shuffle = _mm256_loadu_si256(right_shuffle.as_ptr().cast());
        let right_shifts = _mm256_loadu_si256(right_shifts.as_ptr().cast());
        #[expect(clippy::cast_possible_wrap, reason = "the masks are read unsigned")]
        let (left_mask, right_mask) = (
            _mm256_set1_epi32(((1_u32 << one) - 1) as i32),
            _mm256_set1_epi32(((1_u32 << other) - 1) as i32),
        );
        let shift = _mm256_set1_epi32(shift);
        for (block, word) in words.iter_mut().enumerate() {
            if !fresh && *word == 0 {
                continue;
            }
            let (at, to) =
                (left.as_ptr().add(8 * block * one), right.as_ptr().add(8 * block * other));
            let mut found = 0_u64;
            for group in 0..8 {
                let a = group_codes(
                    at.add(group * one),
                    left_half,
                    left_shuffle,
                    left_shifts,
                    left_mask,
                );
                let b = group_codes(
                    to.add(group * other),
                    right_half,
                    right_shuffle,
                    right_shifts,
                    right_mask,
                );
                let b = _mm256_add_epi32(b, shift);
                let (x, y) = if SWAP { (b, a) } else { (a, b) };
                let test = if EQUAL { _mm256_cmpeq_epi32(x, y) } else { _mm256_cmpgt_epi32(x, y) };
                #[expect(clippy::cast_sign_loss, reason = "eight bits of a movemask")]
                let mut bits = _mm256_movemask_ps(_mm256_castsi256_ps(test)) as u64;
                if NOT {
                    bits ^= 0xff;
                }
                found |= bits << (group * 8);
            }
            *word = if fresh { found } else { *word & found };
        }
    }
}

/// For each way eight answers can fall, the lanes of the ones that are set, lowest first, which is
/// the permute that moves the kept rows of a group to the front of it.
const KEPT_FIRST: [[u32; 8]; 256] = kept_first();

const fn kept_first() -> [[u32; 8]; 256] {
    let mut table = [[0_u32; 8]; 256];
    let mut found = 0;
    while found < 256 {
        let (mut lane, mut kept) = (0, 0);
        while lane < 8 {
            if found >> lane & 1 == 1 {
                #[expect(clippy::cast_possible_truncation, reason = "a lane under eight")]
                {
                    table[found][kept] = lane as u32;
                }
                kept += 1;
            }
            lane += 1;
        }
        found += 1;
    }
    table
}

/// Keeps of `rows` the ones whose code plus `shift`, taken no higher than `range`, is a set bit of
/// `bits`, eight rows at a time, moved down in place and still in order. Returns how many it kept
/// and how many it read, which is every row but the last few, for the caller to finish one at a
/// time.
///
/// Row `r`'s code starts at bit `first + r * width` of `bytes`. Its four bytes and the word of
/// `bits` its offset falls in are each one gather for the group, the code and its bit come out of
/// lanes with a shift and a mask each, and the rows kept are moved to the front of the group with
/// one permute from [`KEPT_FIRST`] and stored where the kept rows end. That store is never past
/// the group it was read from, so the rows still to read are left alone.
///
/// `rows` are in order, so the last row of a group says whether its loads are inside `bytes`. Each
/// position is clamped to the bytes and each offset to `range` all the same, so rows out of order
/// read a wrong code rather than outside the slice. The caller keeps `width` between one and
/// [`LANE_WIDTH_MAX`], `bytes` under 2^28 so a position fits a lane, `range` under 2^31 and inside
/// `bits`, and `shift` under 2^30 either way, so that an offset below zero is above `range` as a
/// `u32` and clamps to it the way the wrapping `u64` does.
#[cfg(all(target_arch = "x86_64", target_feature = "avx2"))]
#[allow(unsafe_code)]
pub(crate) fn retain_set(
    (bytes, first, width): (&[u8], u32, u32),
    rows: &mut [u32],
    bits: &[u64],
    shift: i32,
    range: u32,
) -> (usize, usize) {
    use std::arch::x86_64::{
        _mm256_add_epi32, _mm256_and_si256, _mm256_castsi256_ps, _mm256_i32gather_epi32,
        _mm256_loadu_si256, _mm256_min_epu32, _mm256_movemask_ps, _mm256_mullo_epi32,
        _mm256_permutevar8x32_epi32, _mm256_set1_epi32, _mm256_sllv_epi32, _mm256_srli_epi32,
        _mm256_srlv_epi32, _mm256_storeu_si256, _mm256_sub_epi32,
    };
    assert!((1..=LANE_WIDTH_MAX).contains(&(width as usize)));
    assert!(bytes.len() < 1 << 28 && range < 1 << 31 && (range as usize) < bits.len() * 64);
    assert!(first < 1 << 31 && shift.unsigned_abs() < 1 << 30);
    let Some(limit) = bytes.len().checked_sub(4) else { return (0, 0) };
    let (mut kept, mut at, start) = (0, 0, first as usize);
    // SAFETY: the build enables AVX2, which the `cfg` on this function checks. A group's rows are
    // read and its kept rows written inside `rows`, at `at` and at `kept`, which is never past it.
    // A code's gather is at a byte clamped to `limit`, so its four bytes are inside `bytes`, and a
    // bit's is at a 32 bit word clamped to `range / 32`, which the assert keeps inside `bits`.
    #[expect(clippy::cast_possible_wrap, reason = "every lane is under 2^31")]
    unsafe {
        let codes = bytes.as_ptr().cast::<i32>();
        let words = bits.as_ptr().cast::<i32>();
        let (first, width_lanes) =
            (_mm256_set1_epi32(first as i32), _mm256_set1_epi32(width as i32));
        let mask = _mm256_set1_epi32(((1_u32 << width) - 1) as i32);
        let (shift, range) = (_mm256_set1_epi32(shift), _mm256_set1_epi32(range as i32));
        let (seven, last_bit) = (_mm256_set1_epi32(7), _mm256_set1_epi32(31));
        let top = _mm256_set1_epi32(limit as i32);
        while at + 8 <= rows.len() {
            let last = rows[at + 7] as usize;
            if (start + last * width as usize) / 8 > limit {
                break;
            }
            let row = _mm256_loadu_si256(rows.as_ptr().add(at).cast());
            let bit = _mm256_add_epi32(first, _mm256_mullo_epi32(row, width_lanes));
            let byte = _mm256_min_epu32(_mm256_srli_epi32::<3>(bit), top);
            let code = _mm256_i32gather_epi32::<1>(codes, byte);
            let code =
                _mm256_and_si256(_mm256_srlv_epi32(code, _mm256_and_si256(bit, seven)), mask);
            let offset = _mm256_min_epu32(_mm256_add_epi32(code, shift), range);
            let word = _mm256_i32gather_epi32::<4>(words, _mm256_srli_epi32::<5>(offset));
            let held = _mm256_sllv_epi32(
                word,
                _mm256_sub_epi32(last_bit, _mm256_and_si256(offset, last_bit)),
            );
            #[expect(clippy::cast_sign_loss, reason = "eight bits of a movemask")]
            let found = _mm256_movemask_ps(_mm256_castsi256_ps(held)) as usize;
            let order = _mm256_loadu_si256(KEPT_FIRST[found].as_ptr().cast());
            _mm256_storeu_si256(
                rows.as_mut_ptr().add(kept).cast(),
                _mm256_permutevar8x32_epi32(row, order),
            );
            kept += found.count_ones() as usize;
            at += 8;
        }
    }
    (kept, at)
}

/// Adds each row's `first` and `second` value and a one for its count into the first three cells of
/// its place in `cells`, eight rows at a time, and returns how many rows it added, for the caller to
/// finish the rest one at a time.
///
/// Four rows of each column are one load, and two unpacks make them four pairs of the row's two
/// values, each the low or the high half of a register, so a row is its place, one add of a pair
/// into its cells and one more for its count. A block of eight stops the pass before it is added
/// when one of its places is past `cells`, so the caller meets that place itself.
#[cfg(all(target_arch = "x86_64", target_feature = "avx2"))]
#[allow(unsafe_code)]
pub(crate) fn add_pairs(cells: &mut [[i64; 4]], (first, second): (&[u64], &[u64]), places: &[u32]) -> usize {
    use std::arch::x86_64::{
        _mm_add_epi64, _mm_loadu_si128, _mm_storeu_si128, _mm256_castsi256_si128,
        _mm256_cmpeq_epi32, _mm256_extracti128_si256, _mm256_loadu_si256, _mm256_max_epu32,
        _mm256_movemask_epi8, _mm256_set1_epi32, _mm256_unpackhi_epi64, _mm256_unpacklo_epi64,
    };
    let rows = places.len().min(first.len()).min(second.len());
    let Some(Ok(last)) = cells.len().checked_sub(1).map(u32::try_from) else { return 0 };
    let mut at = 0;
    // SAFETY: the build enables AVX2, which the `cfg` on this function checks. Each block reads
    // eight places and eight values of each column at `at`, which is eight short of `rows` or
    // less. Every place of a block is at most `last` before any of its rows is added, so each
    // row's cells are inside `cells`.
    #[expect(clippy::cast_possible_wrap, reason = "an unsigned compare of the lanes")]
    unsafe {
        let top = _mm256_set1_epi32(last as i32);
        let to = cells.as_mut_ptr();
        while at + 8 <= rows {
            let held = _mm256_loadu_si256(places.as_ptr().add(at).cast());
            if _mm256_movemask_epi8(_mm256_cmpeq_epi32(_mm256_max_epu32(held, top), top)) != -1 {
                break;
            }
            for from in [at, at + 4] {
                let ones = _mm256_loadu_si256(first.as_ptr().add(from).cast());
                let twos = _mm256_loadu_si256(second.as_ptr().add(from).cast());
                let (even, odd) = (_mm256_unpacklo_epi64(ones, twos), _mm256_unpackhi_epi64(ones, twos));
                let pairs = [
                    _mm256_castsi256_si128(even),
                    _mm256_castsi256_si128(odd),
                    _mm256_extracti128_si256::<1>(even),
                    _mm256_extracti128_si256::<1>(odd),
                ];
                for (row, pair) in pairs.into_iter().enumerate() {
                    let cell = to.add(*places.get_unchecked(from + row) as usize).cast::<i64>();
                    _mm_storeu_si128(cell.cast(), _mm_add_epi64(_mm_loadu_si128(cell.cast()), pair));
                    *cell.add(2) = (*cell.add(2)).wrapping_add(1);
                }
            }
            at += 8;
        }
    }
    at
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Pairs added in lanes come to the cells a row at a time would, for places that repeat and a
    /// place past the cells that stops the pass at its block.
    #[cfg(all(target_arch = "x86_64", target_feature = "avx2"))]
    #[test]
    fn pairs_added_in_lanes_are_the_pairs_added_a_row_at_a_time() {
        let rows = 203;
        let first: Vec<u64> = (0..rows as u64).map(|row| row * 7 + 3).collect();
        let second: Vec<u64> = (0..rows as u64).map(|row| (row * 2_654_435_761) % 100_003).collect();
        for past in [None, Some(0), Some(77), Some(200)] {
            let mut places: Vec<u32> = (0..rows as u32).map(|row| (row * 13) % 11).collect();
            if let Some(past) = past {
                places[past] = 11;
            }
            let mut cells = vec![[0_i64; 4]; 11];
            let done = add_pairs(&mut cells, (&first, &second), &places);
            let stop = past.map_or(rows, |past| past / 8 * 8);
            assert_eq!(done, stop / 8 * 8, "{past:?}");
            let mut want = vec![[0_i64; 4]; 11];
            for row in 0..done {
                let cell = &mut want[places[row] as usize];
                cell[0] += first[row] as i64;
                cell[1] += second[row] as i64;
                cell[2] += 1;
            }
            assert_eq!(cells, want, "{past:?}");
        }
    }

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
        for width in 1..=LANE_WIDTH_MAX {
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
    fn blocks_in_one_call_agree_with_a_block_at_a_time() {
        for width in 1..=LANE_WIDTH_MAX {
            let top = (1_u64 << width) - 1;
            let blocks = 5;
            let mut bytes = vec![0_u8; (blocks - 1) * 8 * width + readable(width)];
            let bits = 8 * bytes.len();
            for bit in 0..bits {
                bytes[bit / 8] |= u8::from((bit * 2_654_435_761) % 7 < 3) << (bit % 8);
            }
            let (low, span) = (top / 5, top / 3);
            #[expect(clippy::cast_possible_truncation, reason = "under 2^25")]
            let (low, span) = (low as u32, span as u32);
            let each: Vec<u64> = (0..blocks)
                .map(|block| within(&bytes[8 * block * width..], width, low, span))
                .collect();
            let mut words = vec![0_u64; blocks];
            let kept = within_words(&bytes, width, low, span, &mut words, true);
            assert_eq!(words, each, "width {width}");
            assert_eq!(kept, each.iter().map(|word| word.count_ones() as usize).sum::<usize>());
            let mut words = vec![u64::MAX, 0, 0x5555_5555_5555_5555, u64::MAX, 1];
            let narrowed: Vec<u64> =
                words.iter().zip(&each).map(|(word, held)| word & held).collect();
            within_words(&bytes, width, low, span, &mut words, false);
            assert_eq!(words, narrowed, "width {width} narrowed");
        }
    }

    /// `bytes` holding `blocks` blocks of `width` bit codes and the slack the lanes read past them,
    /// and the codes.
    #[cfg(all(target_arch = "x86_64", target_feature = "avx2"))]
    fn packed_blocks(width: usize, blocks: usize, seed: usize) -> (Vec<u8>, Vec<i64>) {
        let top = (1_usize << width) - 1;
        let codes: Vec<usize> =
            (0..64 * blocks).map(|i| (i * 2_654_435_761 + seed) % 97 % (top + 1)).collect();
        let mut bytes = vec![0_u8; (blocks - 1) * 8 * width + readable(width)];
        for (i, &code) in codes.iter().enumerate() {
            for b in 0..width {
                let bit = i * width + b;
                bytes[bit / 8] |= u8::from(code >> b & 1 == 1) << (bit % 8);
            }
        }
        #[expect(clippy::cast_possible_wrap, reason = "under 2^25")]
        (bytes, codes.into_iter().map(|code| code as i64).collect())
    }

    #[cfg(all(target_arch = "x86_64", target_feature = "avx2"))]
    #[test]
    fn rows_kept_in_lanes_are_the_rows_whose_bit_is_set() {
        for width in [1, 4, 9, 13, 17, 25] {
            let (bytes, codes) = packed_blocks(width, 4, 3);
            for (skip, every) in [(0, 1), (3, 2), (5, 7), (64, 3)] {
                let rows: Vec<u32> = (0..codes.len() - skip)
                    .filter(|row| row % every == 0)
                    .map(|row| u32::try_from(row).unwrap())
                    .collect();
                for (shift, range) in [(0, 96), (-40, 60), (7, 200), (-1, 1), (300, 500)] {
                    let bits: Vec<u64> = (0..=u64::from(range) / 64)
                        .map(|at| (at + 1).wrapping_mul(0x9e37_79b9_7f4a_7c15))
                        .collect();
                    #[expect(clippy::cast_sign_loss, reason = "the wrapping add the domain makes")]
                    let held = |row: &u32| {
                        let code = codes[skip + *row as usize] as u64;
                        let offset = code.wrapping_add(i64::from(shift) as u64).min(range.into());
                        bits[(offset / 64) as usize] >> (offset % 64) & 1 == 1
                    };
                    let first = u32::try_from(skip * width).unwrap();
                    let mut kept_rows = rows.clone();
                    let (kept, read) = retain_set(
                        (&bytes, first, u32::try_from(width).unwrap()),
                        &mut kept_rows,
                        &bits,
                        shift,
                        range,
                    );
                    assert_eq!(read, rows.len() / 8 * 8, "width {width} skip {skip}");
                    let wanted: Vec<u32> = rows[..read].iter().copied().filter(held).collect();
                    assert_eq!(
                        kept_rows[..kept],
                        wanted,
                        "width {width} skip {skip} shift {shift}"
                    );
                }
            }
        }
    }

    #[cfg(all(target_arch = "x86_64", target_feature = "avx2"))]
    #[test]
    fn two_packed_sides_compare_as_their_codes_do() {
        let blocks = 3;
        for (one, other) in [(1, 1), (5, 7), (12, 12), (12, 13), (25, 3), (25, 25)] {
            let (left, a) = packed_blocks(one, blocks, 1);
            let (right, b) = packed_blocks(other, blocks, 5);
            for shift in [-40_i32, -1, 0, 2, 30] {
                let expect = |held: fn(i64, i64) -> bool| -> Vec<u64> {
                    (0..blocks)
                        .map(|block| {
                            (0..64).fold(0, |word, i| {
                                let row = 64 * block + i;
                                word | u64::from(held(a[row], b[row] + i64::from(shift))) << i
                            })
                        })
                        .collect()
                };
                let run = |f: fn((&[u8], usize), (&[u8], usize), i32, &mut [u64], bool)| {
                    let mut words = vec![0_u64; blocks];
                    f((&left, one), (&right, other), shift, &mut words, true);
                    let mut narrowed = vec![u64::MAX, 0, 0x5555_5555_5555_5555];
                    f((&left, one), (&right, other), shift, &mut narrowed, false);
                    (words, narrowed)
                };
                let cases: [(
                    fn((&[u8], usize), (&[u8], usize), i32, &mut [u64], bool),
                    fn(i64, i64) -> bool,
                ); 6] = [
                    (against_words::<true, false, false>, |x, y| x < y),
                    (against_words::<false, false, false>, |x, y| x > y),
                    (against_words::<false, false, true>, |x, y| x <= y),
                    (against_words::<true, false, true>, |x, y| x >= y),
                    (against_words::<false, true, false>, |x, y| x == y),
                    (against_words::<false, true, true>, |x, y| x != y),
                ];
                for (case, (f, held)) in cases.into_iter().enumerate() {
                    let wanted = expect(held);
                    let (words, narrowed) = run(f);
                    assert_eq!(words, wanted, "widths {one} {other} shift {shift} case {case}");
                    let masks = [u64::MAX, 0, 0x5555_5555_5555_5555];
                    let wanted: Vec<u64> = wanted.iter().zip(masks).map(|(w, m)| w & m).collect();
                    assert_eq!(narrowed, wanted, "widths {one} {other} shift {shift} case {case}");
                }
            }
        }
    }

    #[cfg(all(target_arch = "x86_64", target_feature = "avx2"))]
    #[test]
    fn eight_lanes_agree_with_a_code_at_a_time() {
        for width in 1..=LANE_WIDTH_MAX {
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
