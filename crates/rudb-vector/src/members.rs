//! Which of some keys a bitmap holds, one bit a key.
//!
//! A join that knows the keys its build side holds as a bitmap over their range tests every key of
//! the probe side against it before the probe side is read any further. On JOB that test is the
//! hottest loop of the suite, an eighth of every instruction it spends warm, and a third of 29b.
//! One key at a time it is a subtract, a clamp, a load, a shift and an or into the word, about
//! eight instructions a key. With AVX2 it is eight keys a step: a subtract and a clamp over all of
//! them, one gather of the words they land in, a shift that lifts each key's bit into the sign of
//! its lane, and one `movemask` that turns the eight signs into eight bits.
//!
//! The lanes are 32 bits, so this is for `i32` keys over a bitmap of at most 2^30 bits whose base
//! is within 2^30 of zero. Inside those bounds a key under the base wraps round to an offset past
//! the last bit, so one unsigned clamp is the range test at both ends. Past them there is no
//! [`Members`], and the caller tests its keys the way it did before.

/// A bitmap over the keys from a base, ready to test `i32` keys against. See the module docs.
#[derive(Debug, Clone, Copy)]
pub struct Members<'a> {
    words: &'a [u64],
    base: i32,
    /// The offset of the last bit of `words`.
    last: u32,
}

/// The most bits a bitmap may have, and the furthest its base may be from zero, for 32-bit lanes
/// to test it without a key under the base wrapping into it.
const REACH: i64 = 1 << 30;

impl<'a> Members<'a> {
    /// The bitmap `words` with bit 0 standing for the key `base`, or `None` when it is too wide or
    /// its base too far out for 32-bit lanes.
    #[must_use]
    pub fn new(words: &'a [u64], base: i64) -> Option<Self> {
        let bits = i64::try_from(words.len()).ok()?.checked_mul(64)?;
        if words.is_empty() || bits > REACH || !(-REACH..=REACH).contains(&base) {
            return None;
        }
        Some(Self { words, base: i32::try_from(base).ok()?, last: u32::try_from(bits - 1).ok()? })
    }

    /// Bit `i` set when the bitmap holds `keys[i]`, for at most 64 keys.
    #[must_use]
    #[inline]
    pub fn word(&self, keys: &[i32]) -> u64 {
        debug_assert!(keys.len() <= 64, "a word holds 64 keys");
        #[cfg(all(target_arch = "x86_64", target_feature = "avx2"))]
        {
            self.avx2(keys)
        }
        #[cfg(not(all(target_arch = "x86_64", target_feature = "avx2")))]
        {
            self.portable(keys)
        }
    }

    /// Whether the bitmap holds `key`.
    #[must_use]
    #[inline]
    pub fn holds(&self, key: i32) -> bool {
        // Two's complement makes the cast the wrapping subtract's unsigned offset.
        #[allow(clippy::cast_sign_loss)]
        let offset = key.wrapping_sub(self.base) as u32;
        offset <= self.last && self.words[(offset / 64) as usize] >> (offset % 64) & 1 == 1
    }

    /// [`Self::word`] a key at a time.
    #[cfg_attr(all(target_arch = "x86_64", target_feature = "avx2"), allow(dead_code))]
    #[inline]
    fn portable(&self, keys: &[i32]) -> u64 {
        keys.iter()
            .enumerate()
            .fold(0, |word, (bit, &key)| word | u64::from(self.holds(key)) << bit)
    }

    /// [`Self::word`] eight keys a step, and the last few a key at a time.
    #[cfg(all(target_arch = "x86_64", target_feature = "avx2"))]
    #[inline]
    #[allow(unsafe_code)]
    fn avx2(&self, keys: &[i32]) -> u64 {
        use std::arch::x86_64::{
            __m256i, _mm256_and_si256, _mm256_andnot_si256, _mm256_castsi256_ps,
            _mm256_cmpeq_epi32, _mm256_i32gather_epi32, _mm256_loadu_si256, _mm256_min_epu32,
            _mm256_movemask_ps, _mm256_set1_epi32, _mm256_sllv_epi32, _mm256_srli_epi32,
            _mm256_sub_epi32,
        };
        let mut chunks = keys.chunks_exact(8);
        let mut word = 0u64;
        // SAFETY: the build enables AVX2, which the `cfg` on this function checks. Each load reads
        // the eight `i32` of one exact chunk of `keys`, and `loadu` has no alignment requirement.
        // Each gather reads the `i32` at an index the clamp has put at or under `last / 32`, which
        // is the last `i32` of `words` since `last` is its last bit, so every read is inside it.
        unsafe {
            let base = _mm256_set1_epi32(self.base);
            // The clamp is unsigned, so the lane holds the bits of `last` whatever its sign as i32.
            #[allow(clippy::cast_possible_wrap)]
            let last = _mm256_set1_epi32(self.last as i32);
            let low = _mm256_set1_epi32(31);
            let words = self.words.as_ptr().cast::<i32>();
            for (at, eight) in (&mut chunks).enumerate() {
                let keys = _mm256_loadu_si256(eight.as_ptr().cast::<__m256i>());
                let offset = _mm256_sub_epi32(keys, base);
                let clamped = _mm256_min_epu32(offset, last);
                let inside = _mm256_cmpeq_epi32(clamped, offset);
                let lanes = _mm256_i32gather_epi32::<4>(words, _mm256_srli_epi32::<5>(clamped));
                // Shifting left by 31 less the bit's place in its lane puts the bit in the sign.
                let lifted = _mm256_sllv_epi32(lanes, _mm256_andnot_si256(clamped, low));
                let held = _mm256_and_si256(lifted, inside);
                // `movemask` of eight lanes sets only the low eight bits.
                #[allow(clippy::cast_sign_loss)]
                let bits = _mm256_movemask_ps(_mm256_castsi256_ps(held)) as u32;
                word |= u64::from(bits) << (8 * at);
            }
        }
        let done = keys.len() - chunks.remainder().len();
        for (at, &key) in chunks.remainder().iter().enumerate() {
            word |= u64::from(self.holds(key)) << (done + at);
        }
        word
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_word_has_a_bit_for_exactly_the_keys_the_bitmap_holds() {
        let mut seed = 0x9e37_79b9_7f4a_7c15u64;
        let mut next = move || {
            seed ^= seed << 13;
            seed ^= seed >> 7;
            seed ^= seed << 17;
            seed
        };
        for round in 0..500 {
            let len = 1 + (next() % 40) as usize;
            let words: Vec<u64> = (0..len).map(|_| next() & next()).collect();
            let base = (next() % 2001) as i64 - 1000;
            let members = Members::new(&words, base).expect("a small bitmap");
            let count = (next() % 65) as usize;
            // Keys around the range and off both ends, with the extremes of `i32` in as well.
            let keys: Vec<i32> = (0..count)
                .map(|_| match next() % 10 {
                    0 => i32::MIN,
                    1 => i32::MAX,
                    _ => (base + (next() % (len as u64 * 64 + 400)) as i64 - 200) as i32,
                })
                .collect();
            let want = keys.iter().enumerate().fold(0u64, |word, (bit, &key)| {
                let offset = i64::from(key) - base;
                let held = (0..len as i64 * 64).contains(&offset)
                    && words[(offset / 64) as usize] >> (offset % 64) & 1 == 1;
                word | u64::from(held) << bit
            });
            assert_eq!(members.word(&keys), want, "round {round}");
            assert_eq!(members.portable(&keys), want, "round {round}");
        }
    }

    #[test]
    fn a_bitmap_too_far_out_for_the_lanes_has_none() {
        assert!(Members::new(&[], 0).is_none());
        assert!(Members::new(&[1], (1 << 30) + 1).is_none());
        assert!(Members::new(&[1], -(1 << 30) - 1).is_none());
        assert!(Members::new(&[1], 1 << 30).is_some());
        assert!(Members::new(&[1], -(1 << 30)).is_some());
    }
}
