//! Where the runs of a column start, sixty four rows at a time.
//!
//! An aggregate over a key the rows are sorted on closes each run of the key as soon as it ends, and
//! to do that it needs the row every run starts at. A run of `l_orderkey` is four rows, so a branch
//! a row on whether a run starts there guesses wrong a quarter of the time. Instead each row is
//! compared with the one before it into a bit of a word, sixty four at a time, and the starts are
//! read out of the set bits. The same loop checks that the values never go down, which is what lets
//! the aggregate trust that a run is its whole group.
//!
//! The portable walk does a compare and a shift a row. For 64 bit integers with AVX2 four rows are
//! compared at once, equal and greater, and a `movemask` makes four bits of the word, which on TPC-H
//! q18 took the walk from a tenth of the query to a fraction of that.

/// Pushes onto `starts` every row from one up to `values.len()` whose value is not the one before
/// it, and answers true. With `ordered` it answers false as soon as a block of rows goes down, and
/// `starts` then holds whatever it had pushed by then.
pub fn run_starts<T: PartialOrd>(values: &[T], ordered: bool, starts: &mut Vec<u32>) -> bool {
    walk(values, 1, ordered, starts)
}

/// [`run_starts`] for 64 bit integers, four rows at a time where the build has AVX2.
pub fn run_starts_i64(values: &[i64], ordered: bool, starts: &mut Vec<u32>) -> bool {
    #[cfg(all(target_arch = "x86_64", target_feature = "avx2"))]
    {
        let mut row = 1;
        while row + 64 <= values.len() {
            let (mask, down) = differs_i64(values, row);
            if down && ordered {
                return false;
            }
            push(starts, row, mask);
            row += 64;
        }
        walk(values, row, ordered, starts)
    }
    #[cfg(not(all(target_arch = "x86_64", target_feature = "avx2")))]
    {
        walk(values, 1, ordered, starts)
    }
}

/// The portable walk from `row` to the end, `row` at least one.
fn walk<T: PartialOrd>(values: &[T], mut row: usize, ordered: bool, starts: &mut Vec<u32>) -> bool {
    while row < values.len() {
        let end = (row + 64).min(values.len());
        let (before, after) = (&values[row - 1..end - 1], &values[row..end]);
        let (mut mask, mut down) = (0_u64, false);
        for (at, (a, b)) in before.iter().zip(after).enumerate() {
            mask |= u64::from(a != b) << at;
            down |= b < a;
        }
        if down && ordered {
            return false;
        }
        push(starts, row, mask);
        row = end;
    }
    true
}

/// A start for each bit set in `mask`, bit `i` being row `row + i`.
#[inline]
fn push(starts: &mut Vec<u32>, row: usize, mut mask: u64) {
    while mask != 0 {
        #[expect(clippy::cast_possible_truncation, reason = "a chunk is far under 2^32 rows")]
        starts.push((row + mask.trailing_zeros() as usize) as u32);
        mask &= mask - 1;
    }
}

/// A bit for each of the 64 rows from `row` whose value is not the one before it, and whether any
/// of them is less than the one before it. `row` is at least one and `row + 64` at most the length.
#[cfg(all(target_arch = "x86_64", target_feature = "avx2"))]
#[inline]
#[allow(unsafe_code)]
fn differs_i64(values: &[i64], row: usize) -> (u64, bool) {
    use std::arch::x86_64::{
        _mm256_castsi256_pd, _mm256_cmpeq_epi64, _mm256_cmpgt_epi64, _mm256_loadu_si256,
        _mm256_movemask_pd, _mm256_or_si256, _mm256_setzero_si256, _mm256_testz_si256,
    };
    assert!(row >= 1 && row + 64 <= values.len());
    let mut mask = 0_u64;
    // SAFETY: the build enables AVX2, which the `cfg` on this function checks. Step `k` loads four
    // values at `row - 1 + 4k` and four at `row + 4k`, and the last ends at `row + 64`, which the
    // assert keeps inside `values`. `loadu` has no alignment requirement.
    unsafe {
        let at = values.as_ptr().add(row);
        let mut down = _mm256_setzero_si256();
        for k in 0..16 {
            let before = _mm256_loadu_si256(at.add(4 * k).sub(1).cast());
            let after = _mm256_loadu_si256(at.add(4 * k).cast());
            let same = _mm256_movemask_pd(_mm256_castsi256_pd(_mm256_cmpeq_epi64(before, after)));
            #[expect(clippy::cast_sign_loss, reason = "four bits of a movemask")]
            let differ = (!same & 0xf) as u64;
            mask |= differ << (4 * k);
            down = _mm256_or_si256(down, _mm256_cmpgt_epi64(before, after));
        }
        (mask, _mm256_testz_si256(down, down) == 0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[expect(clippy::cast_possible_truncation, reason = "a thousand rows")]
    fn walked(values: &[i64]) -> Vec<u32> {
        (1..values.len()).filter(|&row| values[row] != values[row - 1]).map(|row| row as u32).collect()
    }

    /// Both walks find the starts a plain loop does, across blocks and with runs longer than a block,
    /// and with values at both ends of the range.
    #[test]
    fn the_starts_are_where_the_value_changes() {
        let mut values: Vec<i64> = (0..1_000_i64).map(|row| row / 3 + row / 70 * 40).collect();
        values[..5].fill(i64::MIN);
        values[995..].fill(i64::MAX);
        for rows in [0, 1, 2, 63, 64, 65, 128, 129, 1_000] {
            let values = &values[..rows];
            for ordered in [false, true] {
                let mut fast = Vec::new();
                assert!(run_starts_i64(values, ordered, &mut fast));
                assert_eq!(fast, walked(values), "{rows} rows");
                let mut slow = Vec::new();
                assert!(run_starts(values, ordered, &mut slow));
                assert_eq!(slow, fast, "{rows} rows");
            }
        }
    }

    /// A value that goes down anywhere is refused when the order is asked for, in the vector blocks
    /// and in the tail, and found like any other change when it is not.
    #[test]
    fn going_down_is_refused_only_when_order_is_asked_for() {
        for at in [1, 40, 64, 65, 127, 200, 299] {
            let mut values: Vec<i64> = (0..300_i64).map(|row| row / 4).collect();
            values[at] = -7;
            let mut starts = Vec::new();
            assert!(!run_starts_i64(&values, true, &mut starts), "down at {at}");
            assert!(!run_starts(&values, true, &mut Vec::new()), "down at {at}");
            starts.clear();
            assert!(run_starts_i64(&values, false, &mut starts));
            assert_eq!(starts, walked(&values), "down at {at}");
        }
    }
}
