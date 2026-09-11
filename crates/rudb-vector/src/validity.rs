//! Which values in a vector are not null.
//!
//! `spec/07-execution.md` section 7.1: validity has three representations and the distinction is
//! load-bearing. All valid is the absence of a mask and gets the fastest kernels. All invalid is a
//! flag and short circuits entirely. Anything else is a bitmap.
//!
//! Photon's published result is that separate no-null kernels are worth a measurable amount on
//! real data, because real data is mostly not null. The cost of knowing which case you are in is
//! one branch per vector rather than one per value, which is why the three cases are an enum here
//! rather than a bitmap that happens to be all ones.

/// Which values in a vector are valid.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Validity {
    /// Nothing is null. No mask is stored and the kernels that read this take the fast path.
    AllValid,
    /// Everything is null. Most operators can answer without looking at the data at all.
    AllInvalid,
    /// Some of each, one bit per value, set meaning valid.
    Mask(Bitmap),
}

impl Validity {
    /// How many bytes of memory this representation is holding.
    ///
    /// The two cheap arms hold none at all, which is the point of having them.
    #[must_use]
    pub fn footprint(&self) -> usize {
        match self {
            Self::AllValid | Self::AllInvalid => 0,
            Self::Mask(mask) => mask.footprint(),
        }
    }

    /// Whether the value at `index` is not null.
    ///
    /// Out of range reads report invalid rather than panicking, because this is called from
    /// kernels that are allowed to read past the end of a partially filled vector.
    #[must_use]
    pub fn is_valid(&self, index: usize) -> bool {
        match self {
            Self::AllValid => true,
            Self::AllInvalid => false,
            Self::Mask(mask) => mask.get(index),
        }
    }

    /// Whether any value in the first `len` is null.
    #[must_use]
    pub fn has_nulls(&self, len: usize) -> bool {
        match self {
            Self::AllValid => false,
            Self::AllInvalid => len > 0,
            Self::Mask(mask) => mask.count_valid(len) != len,
        }
    }

    /// How many of the first `len` values are not null.
    #[must_use]
    pub fn count_valid(&self, len: usize) -> usize {
        match self {
            Self::AllValid => len,
            Self::AllInvalid => 0,
            Self::Mask(mask) => mask.count_valid(len),
        }
    }

    /// Collapses a mask that turned out to be uniform back to one of the flag cases.
    ///
    /// Worth doing at the end of any operation that builds a mask, because every kernel
    /// downstream then gets to take the branch it wants rather than walking a bitmap to find out
    /// what it already could have been told.
    #[must_use]
    pub fn normalize(self, len: usize) -> Self {
        match self {
            Self::Mask(ref mask) => {
                let valid = mask.count_valid(len);
                if valid == len {
                    Self::AllValid
                } else if valid == 0 {
                    Self::AllInvalid
                } else {
                    self
                }
            }
            other => other,
        }
    }

    /// The validity of a vector where `index` has just been made null.
    ///
    /// Takes and returns by value because setting a null on an `AllValid` vector has to
    /// materialize a mask, and hiding that behind `&mut self` hides an allocation.
    #[must_use]
    pub fn with_null(self, index: usize, len: usize) -> Self {
        let mut mask = match self {
            Self::AllValid => Bitmap::all_valid(len),
            Self::AllInvalid => return Self::AllInvalid,
            Self::Mask(mask) => mask,
        };
        mask.set(index, false);
        Self::Mask(mask)
    }

    /// Validity built from a per-value predicate, normalized.
    pub fn from_iter(len: usize, valid: impl Fn(usize) -> bool) -> Self {
        let mut mask = Bitmap::all_valid(len);
        for index in 0..len {
            if !valid(index) {
                mask.set(index, false);
            }
        }
        Self::Mask(mask).normalize(len)
    }

    /// Validity packed from one byte a row, which is what a kernel that accumulated its answer in a
    /// `Vec<bool>` is holding when it finishes.
    ///
    /// The difference from [`Self::from_iter`] is the shape rather than the answer. `from_iter`
    /// calls a closure and then a read modify write on a byte of the bitmap, once per row, and the
    /// read modify write is a dependency on the row before it. This reads sixty four bytes and
    /// writes one word, which has no dependency in it at all and is what the compiler needs to see
    /// before it will use a vector instruction. On a thousand row vector that is the difference
    /// between two nanoseconds a row and something too small to measure.
    /// The bits past the end of the last word are set rather than clear, which looks like a detail
    /// and is not. [`Bitmap`] does not carry a length, so its equality is over whole words, and
    /// [`Bitmap::all_valid`] leaves those bits set. A constructor that left them clear would build
    /// a validity that says exactly the same thing about every row that exists and still compares
    /// unequal to the one [`Self::from_iter`] builds, which is a test failure with no wrong answer
    /// in it and an afternoon to work out.
    #[must_use]
    pub fn from_run(valid: &[bool]) -> Self {
        let len = valid.len();
        let mut words = vec![0u64; len.div_ceil(64)];
        for (word, run) in words.iter_mut().zip(valid.chunks(64)) {
            // Only the last run can be short, and the shift is written around rather than as
            // `u64::MAX << 64`, which is not a shift this machine has.
            let mut packed = if run.len() == 64 { 0 } else { u64::MAX << run.len() };
            for (bit, &live) in run.iter().enumerate() {
                packed |= u64::from(live) << bit;
            }
            *word = packed;
        }
        Self::Mask(Bitmap { words }).normalize(len)
    }

    /// The validity of a value that is valid in both inputs, which is what almost every binary
    /// operator wants and is worth having in one place.
    #[must_use]
    pub fn and(&self, other: &Self, len: usize) -> Self {
        match (self, other) {
            (Self::AllInvalid, _) | (_, Self::AllInvalid) => Self::AllInvalid,
            (Self::AllValid, Self::AllValid) => Self::AllValid,
            (Self::AllValid, right) => right.clone().normalize(len),
            (left, Self::AllValid) => left.clone().normalize(len),
            (Self::Mask(left), Self::Mask(right)) => {
                let mut result = left.clone();
                result.and_with(right);
                Self::Mask(result).normalize(len)
            }
        }
    }
}

/// One bit per value, set meaning valid.
///
/// Words are `u64` because that is the width the popcount and the mask tests want, and because a
/// 1024 value vector is exactly 16 of them, which fits in a quarter of a cache line pair and is
/// the reason the vector size is 1024 rather than DuckDB's 2048.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Bitmap {
    words: Vec<u64>,
}

impl Bitmap {
    /// How many bytes of memory this bitmap is holding.
    #[must_use]
    pub fn footprint(&self) -> usize {
        self.words.capacity() * size_of::<u64>()
    }

    /// A bitmap with room for `len` values, all valid.
    #[must_use]
    pub fn all_valid(len: usize) -> Self {
        Self { words: vec![u64::MAX; len.div_ceil(64)] }
    }

    /// A bitmap with room for `len` values, all null.
    #[must_use]
    pub fn all_invalid(len: usize) -> Self {
        Self { words: vec![0; len.div_ceil(64)] }
    }

    /// Whether the value at `index` is valid. Past the end reads as invalid.
    #[must_use]
    pub fn get(&self, index: usize) -> bool {
        let word = index / 64;
        self.words.get(word).is_some_and(|w| w >> (index % 64) & 1 == 1)
    }

    /// Sets whether the value at `index` is valid, growing the bitmap if it has to.
    pub fn set(&mut self, index: usize, valid: bool) {
        let word = index / 64;
        if word >= self.words.len() {
            self.words.resize(word + 1, 0);
        }
        let bit = 1u64 << (index % 64);
        if valid {
            self.words[word] |= bit;
        } else {
            self.words[word] &= !bit;
        }
    }

    /// How many of the first `len` values are valid.
    #[must_use]
    pub fn count_valid(&self, len: usize) -> usize {
        let mut count = 0usize;
        let full_words = len / 64;
        for word in self.words.iter().take(full_words) {
            count += word.count_ones() as usize;
        }
        let tail = len % 64;
        if tail > 0 {
            // A let chain would read better here, but let chains want Rust 1.88 and the declared
            // minimum in the manifest is 1.85.0. Written the long way rather than moving the
            // minimum, since nothing about this needs a newer compiler.
            if let Some(word) = self.words.get(full_words) {
                // Mask off the bits past the end, which are whatever the last resize left there.
                let keep = u64::MAX >> (64 - tail);
                count += (word & keep).count_ones() as usize;
            }
        }
        count
    }

    /// Sixty four validity bits at once, the lowest numbered row in the lowest bit.
    ///
    /// Past the end reads as all null, which is the same answer [`Self::get`] gives one bit at a
    /// time. This exists because a kernel that asks [`Self::get`] once per row pays a bounds check,
    /// a divide and a shift for each of them, and the word it wants was already in a register for
    /// the previous sixty three. A loop that reads the word once and walks its bits is the same
    /// answer at a fraction of the cost, and the three call sites that do that are the difference
    /// between a nullable column being free and being the slowest thing in the kernel.
    #[must_use]
    pub fn word(&self, at: usize) -> u64 {
        self.words.get(at).copied().unwrap_or(0)
    }

    /// Intersects this bitmap with another, in place.
    pub fn and_with(&mut self, other: &Self) {
        for (index, word) in self.words.iter_mut().enumerate() {
            *word &= other.words.get(index).copied().unwrap_or(0);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{Bitmap, Validity};

    #[test]
    fn the_three_cases_answer_the_same_question_the_same_way() {
        let mut mask = Bitmap::all_valid(8);
        assert!(Validity::AllValid.is_valid(3));
        assert!(!Validity::AllInvalid.is_valid(3));
        assert!(Validity::Mask(mask.clone()).is_valid(3));
        mask.set(3, false);
        assert!(!Validity::Mask(mask).is_valid(3));
    }

    #[test]
    fn a_uniform_mask_collapses_to_the_flag_it_should_have_been() {
        // The point of doing this at the end of every operation that builds a mask: the kernel
        // downstream gets to branch once rather than walk a bitmap to learn what it was told.
        assert_eq!(Validity::Mask(Bitmap::all_valid(64)).normalize(64), Validity::AllValid);
        assert_eq!(Validity::Mask(Bitmap::all_invalid(64)).normalize(64), Validity::AllInvalid);
        let mut mask = Bitmap::all_valid(64);
        mask.set(7, false);
        assert!(matches!(Validity::Mask(mask).normalize(64), Validity::Mask(_)));
    }

    #[test]
    fn a_word_of_validity_says_the_same_thing_the_bits_do_one_at_a_time() {
        let mut mask = Bitmap::all_valid(200);
        mask.set(0, false);
        mask.set(63, false);
        mask.set(64, false);
        mask.set(199, false);
        for index in 0..200 {
            let from_word = mask.word(index / 64) >> (index % 64) & 1 == 1;
            assert_eq!(from_word, mask.get(index), "{index}");
        }
        // Past the end is all null, which is what reading one bit past the end says too.
        assert_eq!(mask.word(9), 0);
        assert!(!mask.get(9 * 64));
    }

    #[test]
    fn packing_a_run_of_bytes_says_the_same_thing_as_setting_the_bits() {
        // Two lengths that are not a whole number of words, because the bits past the end of the
        // last word are the part of this that is easy to get wrong.
        for len in [0, 1, 63, 64, 65, 100, 1024] {
            let live: Vec<bool> = (0..len).map(|index| index % 7 != 0).collect();
            let packed = Validity::from_run(&live);
            let set = Validity::from_iter(len, |index| live[index]);
            assert_eq!(packed, set, "{len}");
            for (index, &want) in live.iter().enumerate() {
                assert_eq!(packed.is_valid(index), want, "{len} at {index}");
            }
        }
        // The bits past the end of the last word have to match what every other constructor
        // leaves there, because a bitmap does not carry a length and its equality is over whole
        // words. This is the assertion that caught it.
        assert_eq!(
            Validity::from_run(&[true, false, true]),
            Validity::from_iter(3, |index| index != 1)
        );
        // And it collapses the uniform cases the same way everything else does.
        assert_eq!(Validity::from_run(&[true; 64]), Validity::AllValid);
        assert_eq!(Validity::from_run(&[false; 64]), Validity::AllInvalid);
        assert_eq!(Validity::from_run(&[]), Validity::AllValid);
    }

    #[test]
    fn counting_stops_at_the_length_and_not_at_the_word_boundary() {
        // A 1024 vector is 16 words exactly, but a partially filled one is not, and the bits past
        // the end are whatever the last resize left there. Getting this wrong makes a count that
        // is right in tests of length 64 and wrong on real data.
        let mask = Bitmap::all_valid(100);
        assert_eq!(mask.count_valid(100), 100);
        assert_eq!(mask.count_valid(65), 65);
        assert_eq!(mask.count_valid(1), 1);
        assert_eq!(mask.count_valid(0), 0);
    }

    #[test]
    fn setting_a_null_on_an_all_valid_vector_materializes_a_mask() {
        let validity = Validity::AllValid.with_null(5, 64);
        assert!(!validity.is_valid(5));
        assert!(validity.is_valid(4));
        assert_eq!(validity.count_valid(64), 63);
        assert!(validity.has_nulls(64));
    }

    #[test]
    fn setting_a_null_on_an_all_invalid_vector_changes_nothing() {
        assert_eq!(Validity::AllInvalid.with_null(5, 64), Validity::AllInvalid);
    }

    #[test]
    fn intersection_short_circuits_on_the_flags() {
        let mut left = Bitmap::all_valid(8);
        left.set(0, false);
        let mut right = Bitmap::all_valid(8);
        right.set(1, false);
        let both = Validity::Mask(left.clone()).and(&Validity::Mask(right), 8);
        assert!(!both.is_valid(0));
        assert!(!both.is_valid(1));
        assert!(both.is_valid(2));
        assert_eq!(both.count_valid(8), 6);

        assert_eq!(Validity::AllValid.and(&Validity::AllValid, 8), Validity::AllValid);
        assert_eq!(Validity::AllInvalid.and(&Validity::Mask(left), 8), Validity::AllInvalid);
    }

    #[test]
    fn validity_from_a_predicate_normalizes_itself() {
        assert_eq!(Validity::from_iter(16, |_| true), Validity::AllValid);
        assert_eq!(Validity::from_iter(16, |_| false), Validity::AllInvalid);
        let mixed = Validity::from_iter(16, |i| i % 2 == 0);
        assert_eq!(mixed.count_valid(16), 8);
    }
}
