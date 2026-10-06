//! The bloom filter of a run: 10 bits a key and 7 probes, about 1% false positives (section 11.6).

/// Bits the filter spends on each key.
const BITS_PER_KEY: usize = 10;
/// Bits a key sets and a probe tests.
pub(crate) const PROBES: u32 = 7;

/// A bloom filter over normalized keys.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Bloom {
    words: Box<[u64]>,
}

impl Bloom {
    /// An empty filter sized for `keys` keys.
    pub(crate) fn sized(keys: usize) -> Self {
        let words = (keys.max(1) * BITS_PER_KEY).div_ceil(64);
        Self { words: vec![0; words].into() }
    }

    /// A filter over the words a run kept.
    pub(crate) fn from_words(words: Box<[u64]>) -> Option<Self> {
        (!words.is_empty()).then_some(Self { words })
    }

    pub(crate) fn words(&self) -> &[u64] {
        &self.words
    }

    /// Adds the key whose [`hash`] is `hash`.
    pub(crate) fn insert(&mut self, hash: u64) {
        let bits = self.words.len() as u64 * 64;
        for bit in probes(hash, bits) {
            self.words[(bit / 64) as usize] |= 1 << (bit % 64);
        }
    }

    /// Whether the key whose [`hash`] is `hash` may have been added. `false` is certain.
    pub(crate) fn may_hold(&self, hash: u64) -> bool {
        let bits = self.words.len() as u64 * 64;
        probes(hash, bits).all(|bit| self.words[(bit / 64) as usize] & (1 << (bit % 64)) != 0)
    }
}

/// The bits a key with `hash` sets, by double hashing: the low and the high half of the hash make
/// every probe.
fn probes(hash: u64, bits: u64) -> impl Iterator<Item = u64> {
    let low = hash & 0xFFFF_FFFF;
    let high = (hash >> 32) | 1;
    (0..u64::from(PROBES)).map(move |i| low.wrapping_add(i.wrapping_mul(high)) % bits)
}

/// A 64-bit hash of a key, eight bytes at a time with a final mix.
pub(crate) fn hash(key: &[u8]) -> u64 {
    const K: u64 = 0x9E37_79B9_7F4A_7C15;
    let mut h = (key.len() as u64).wrapping_mul(K);
    let mut words = key.chunks_exact(8);
    for word in &mut words {
        let word = u64::from_le_bytes(word.try_into().expect("eight bytes"));
        h = (h ^ word).wrapping_mul(K).rotate_left(29);
    }
    let mut tail = [0; 8];
    tail[..words.remainder().len()].copy_from_slice(words.remainder());
    h = (h ^ u64::from_le_bytes(tail)).wrapping_mul(K);
    // The finalizer of MurmurHash3, so every bit of the input reaches both halves.
    h ^= h >> 33;
    h = h.wrapping_mul(0xFF51_AFD7_ED55_8CCD);
    h ^= h >> 33;
    h = h.wrapping_mul(0xC4CE_B9FE_1A85_EC53);
    h ^ (h >> 33)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn holds_what_it_was_given_and_little_else() {
        let n = 20_000;
        let mut bloom = Bloom::sized(n);
        for i in 0..n {
            bloom.insert(hash(format!("user{i}").as_bytes()));
        }
        assert!((0..n).all(|i| bloom.may_hold(hash(format!("user{i}").as_bytes()))));
        let false_positives =
            (n..2 * n).filter(|i| bloom.may_hold(hash(format!("user{i}").as_bytes()))).count();
        // About 1% is expected. Twice that would say the hash or the probes are off.
        assert!(false_positives < n / 50, "{false_positives} false positives in {n}");
    }
}
