//! Where some bytes are in a block of sixty four, one bit a byte.
//!
//! This is the step a structural scan starts from, the one simdjson and simdcsv take: compare a
//! block against a few bytes of interest and get a `u64` per byte, first byte in the lowest bit.
//! The compares vectorize by themselves. Turning sixteen compare results into sixteen bits does
//! not, because the portable way to gather them is a multiply per eight bytes and the compiler does
//! not know that `pmovmskb` does it in one instruction. On a `lineitem` load from CSV the gathering
//! was more than half of the splitter's time.
//!
//! So on `x86_64` the gather is SSE2's `movemask`, which every `x86_64` processor has, and there is
//! nothing to detect at run time. Everywhere else it is the portable multiply, which is also what
//! the tests hold the SSE2 version to.

/// One mask per byte in `needles`, with bit `i` of mask `n` set when `block[i] == needles[n]`.
#[must_use]
#[inline]
pub fn masks<const N: usize>(block: &[u8; 64], needles: [u8; N]) -> [u64; N] {
    #[cfg(target_arch = "x86_64")]
    {
        sse2(block, needles)
    }
    #[cfg(not(target_arch = "x86_64"))]
    {
        portable(block, needles)
    }
}

/// [`masks`] with SSE2: four loads of sixteen bytes, then a compare and a `movemask` per needle and
/// load.
#[cfg(target_arch = "x86_64")]
#[inline]
#[allow(unsafe_code)]
fn sse2<const N: usize>(block: &[u8; 64], needles: [u8; N]) -> [u64; N] {
    use std::arch::x86_64::{
        __m128i, _mm_cmpeq_epi8, _mm_loadu_si128, _mm_movemask_epi8, _mm_set1_epi8,
    };
    // SAFETY: SSE2 is part of the `x86_64` baseline, so every processor this code can run on has
    // these instructions. Each load reads sixteen bytes starting at offset 0, 16, 32 or 48 of a
    // sixty four byte array, all inside it, and `loadu` has no alignment requirement.
    unsafe {
        let base = block.as_ptr().cast::<__m128i>();
        let lanes = [
            _mm_loadu_si128(base),
            _mm_loadu_si128(base.add(1)),
            _mm_loadu_si128(base.add(2)),
            _mm_loadu_si128(base.add(3)),
        ];
        needles.map(|needle| {
            // The needle is a byte and `set1` takes the same bits as an `i8`.
            #[allow(clippy::cast_possible_wrap)]
            let splat = _mm_set1_epi8(needle as i8);
            let mut mask = 0u64;
            for (at, lane) in lanes.iter().enumerate() {
                // `movemask` of bytes sets only the low sixteen bits, so the cast keeps them all.
                #[allow(clippy::cast_sign_loss)]
                let bits = _mm_movemask_epi8(_mm_cmpeq_epi8(*lane, splat)) as u32 as u64;
                mask |= bits << (16 * at);
            }
            mask
        })
    }
}

/// [`masks`] without a platform's instructions. A compare per byte, which becomes vector compares,
/// and then eight of those gathered into eight bits with one multiply.
#[cfg_attr(target_arch = "x86_64", allow(dead_code))]
#[inline]
fn portable<const N: usize>(block: &[u8; 64], needles: [u8; N]) -> [u64; N] {
    needles.map(|needle| {
        let mut hits = [0u8; 64];
        for (hit, &byte) in hits.iter_mut().zip(block) {
            *hit = u8::from(byte == needle);
        }
        let mut mask = 0u64;
        for (at, eight) in hits.chunks_exact(8).enumerate() {
            let word = u64::from_le_bytes(eight.try_into().expect("eight bytes"));
            // The multiply puts a copy of byte `i` at bit `56 + i` for every `i` at once, and every
            // other copy it makes lands on a bit of its own below bit 56, so nothing carries into
            // the top byte.
            mask |= (word.wrapping_mul(0x0102_0408_1020_4080) >> 56) << (8 * at);
        }
        mask
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn by_hand(block: &[u8; 64], needle: u8) -> u64 {
        let mut mask = 0;
        for (at, &byte) in block.iter().enumerate() {
            if byte == needle {
                mask |= 1 << at;
            }
        }
        mask
    }

    #[test]
    fn every_mask_has_a_bit_for_exactly_the_bytes_that_match() {
        let mut seed = 0x9e37_79b9_7f4a_7c15u64;
        for round in 0..2000 {
            let mut block = [0u8; 64];
            for byte in &mut block {
                seed ^= seed << 13;
                seed ^= seed >> 7;
                seed ^= seed << 17;
                // A small alphabet so that every needle turns up, with the high bit set half the
                // time so that a byte that is negative as an `i8` is covered.
                *byte = [b',', b'"', b'\n', b'\r', b'a', 0x80, 0xff, 0][(seed % 8) as usize];
            }
            let needles = [b',', b'"', b'\n', b'\r', 0xff, 0, b'z'];
            let want = needles.map(|needle| by_hand(&block, needle));
            assert_eq!(masks(&block, needles), want, "round {round}");
            assert_eq!(portable(&block, needles), want, "round {round}");
        }
    }
}
