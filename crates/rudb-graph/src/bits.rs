//! A bitmap, a rank index over it, and select in both directions.
//!
//! The rank index was written for the dense key map of spec/graph/03-the-file-format.md section 3.3
//! and lived inside `keymap.rs` until the monotone forward link of section 3.4 needed the same
//! structure with select added. It is here rather than there because two callers with different
//! reasons is what a module is for, and because the second caller needs an operation the first does
//! not: a key map only ever asks how many keys are below this one, and a monotone link asks where
//! the nth one is.
//!
//! [`Rank`] is unchanged by the move, including its serialized bytes, so a file written before it
//! reads the same after. [`BitVector`] is the new part: it owns a bitmap, a [`Rank`] over it, and
//! the sampling that makes select a bounded search rather than a scan.
//!
//! # What select costs and why it is not stored
//!
//! Section 3.4 budgets "a sampled select structure of one position every four thousand ninety six
//! ones plus a two-level rank index" at about thirteen percent over the bitmap. The rank index is
//! stored, because rebuilding it is a pass over ninety four megabytes at SF100 and that is a thing
//! you notice at open time. The samples are not stored, because rebuilding them is a pass over the
//! superblock array, which at SF100 is a hundred and eighty three thousand entries and is not.
//! A number derivable in a microsecond is a number that should not be given the chance to disagree
//! with the array it describes.

use rudb_common::{Error, Result};

/// Bits in one rank superblock.
const SUPERBLOCK_BITS: usize = 4096;

/// Bits in one rank block.
const BLOCK_BITS: usize = 512;

/// Blocks in one superblock.
const BLOCKS_PER_SUPERBLOCK: usize = SUPERBLOCK_BITS / BLOCK_BITS;

/// Words in one rank block.
const BLOCK_WORDS: usize = BLOCK_BITS / 64;

/// One select sample every this many ones, and every this many zeros.
///
/// Section 3.4's number. It is a window for a binary search rather than an answer, so a larger
/// sample costs search steps and not correctness, which is why it can be moved without a format
/// change: the samples are rebuilt at open.
const SAMPLE: u64 = 4096;

/// A two level rank index over a bitmap.
///
/// Superblocks of 4096 bits hold a `u32` cumulative count from the start of the bitmap, and blocks
/// of 512 bits hold a `u16` count from the start of their superblock. A rank is then two loads and
/// a `popcount` over at most eight words, which is section 3.3's arithmetic and is the reason the
/// block size is 512: a `u16` cannot hold a count over a wider superblock than 4096, and eight
/// words is the most a `popcount` loop should have to do.
#[derive(Debug, Clone)]
pub(crate) struct Rank {
    superblocks: Vec<u32>,
    blocks: Vec<u16>,
}

impl Rank {
    pub(crate) fn build(bits: &[u64]) -> Self {
        let blocks = bits.len().div_ceil(BLOCK_WORDS);
        let mut index = Self {
            superblocks: Vec::with_capacity(blocks.div_ceil(BLOCKS_PER_SUPERBLOCK)),
            blocks: Vec::with_capacity(blocks),
        };
        let mut total = 0_u32;
        let mut within = 0_u16;
        for block in 0..blocks {
            if block % BLOCKS_PER_SUPERBLOCK == 0 {
                index.superblocks.push(total);
                within = 0;
            }
            index.blocks.push(within);
            let words = block * BLOCK_WORDS;
            let ones: u32 = bits[words..(words + BLOCK_WORDS).min(bits.len())]
                .iter()
                .map(|word| word.count_ones())
                .sum();
            total += ones;
            // A superblock holds at most 4096 ones, so this cannot overflow a u16, and the `as` is
            // guarded by the reset above rather than by hope.
            #[expect(
                clippy::cast_possible_truncation,
                reason = "a superblock holds at most 4096 bits, which fits a u16"
            )]
            let ones = ones as u16;
            within += ones;
        }
        index
    }

    /// How many bits are set strictly below `at`.
    pub(crate) fn rank(&self, bits: &[u64], at: usize) -> u64 {
        let block = at / BLOCK_BITS;
        let superblock = block / BLOCKS_PER_SUPERBLOCK;
        let mut count = u64::from(self.superblocks[superblock]) + u64::from(self.blocks[block]);
        let from = block * BLOCK_WORDS;
        let word = at / 64;
        for whole in &bits[from..word] {
            count += u64::from(whole.count_ones());
        }
        let remainder = at % 64;
        if remainder != 0 {
            let mask = (1_u64 << remainder) - 1;
            count += u64::from((bits[word] & mask).count_ones());
        }
        count
    }

    pub(crate) fn bytes(&self) -> usize {
        self.superblocks.len() * size_of::<u32>() + self.blocks.len() * size_of::<u16>()
    }

    /// How many blocks and superblocks index a bitmap of this many words.
    ///
    /// Derived rather than stored, because both counts are a function of the range the header
    /// already carries and a stored count is a count that can disagree with the array it describes.
    pub(crate) fn shape(words: usize) -> (usize, usize) {
        let blocks = words.div_ceil(BLOCK_WORDS);
        (blocks, blocks.div_ceil(BLOCKS_PER_SUPERBLOCK))
    }

    pub(crate) fn write(&self, out: &mut Vec<u8>) {
        for count in &self.superblocks {
            out.extend_from_slice(&count.to_le_bytes());
        }
        for offset in &self.blocks {
            out.extend_from_slice(&offset.to_le_bytes());
        }
    }

    /// Reads an index over a bitmap of `words` words from exactly the bytes it takes.
    pub(crate) fn read(bytes: &[u8], words: usize) -> Result<Self> {
        let (blocks, superblocks) = Self::shape(words);
        let split = superblocks * size_of::<u32>();
        if bytes.len() != split + blocks * size_of::<u16>() {
            return Err(malformed(
                "a dense key map's rank index is not the size its range implies",
            ));
        }
        Ok(Self {
            superblocks: bytes[..split]
                .chunks_exact(size_of::<u32>())
                .map(|word| u32::from_le_bytes(word.try_into().expect("four bytes")))
                .collect(),
            blocks: bytes[split..]
                .chunks_exact(size_of::<u16>())
                .map(|word| u16::from_le_bytes(word.try_into().expect("two bytes")))
                .collect(),
        })
    }

    /// Ones strictly before the first bit of a superblock.
    fn ones_before_superblock(&self, superblock: usize) -> u64 {
        u64::from(self.superblocks[superblock])
    }

    /// Ones strictly before the first bit of a block.
    fn ones_before_block(&self, block: usize) -> u64 {
        u64::from(self.superblocks[block / BLOCKS_PER_SUPERBLOCK]) + u64::from(self.blocks[block])
    }
}

/// A bitmap with rank and select, in both directions.
///
/// The monotone forward link of section 3.4 asks all four questions of one vector: `rank0` and
/// `select1` answer `forward`, and `select0` answers `backward`. That is the whole reason the
/// monotone form replaces both a forward link and a backward adjacency rather than only the
/// forward one.
#[derive(Debug, Clone)]
pub struct BitVector {
    words: Vec<u64>,
    /// Bits that mean something. The tail of the last word is zero and is not one of them.
    len: usize,
    ones: u64,
    rank: Rank,
    /// Superblock holding the `SAMPLE * k`th one, for each `k`. Rebuilt at read, never stored.
    ones_sample: Vec<u32>,
    /// The same for zeros.
    zeros_sample: Vec<u32>,
}

impl BitVector {
    /// Builds a vector over `len` bits held in `words`, least significant bit of word zero first.
    ///
    /// # Errors
    ///
    /// If `words` is not exactly the number of words `len` bits take, or if a bit is set in the
    /// tail past `len`. The second one matters: a set tail bit is counted by `rank` and found by
    /// `select`, so accepting it would mean a vector that answers questions about bits nobody
    /// wrote.
    pub fn new(words: Vec<u64>, len: usize) -> Result<Self> {
        if words.len() != len.div_ceil(64) {
            return Err(malformed("a bit vector's words do not match its length"));
        }
        let tail = len % 64;
        if tail != 0 && words[len / 64] >> tail != 0 {
            return Err(malformed("a bit vector has bits set past its length"));
        }
        let rank = Rank::build(&words);
        let ones = words.iter().map(|word| u64::from(word.count_ones())).sum();
        let mut vector =
            Self { words, len, ones, rank, ones_sample: Vec::new(), zeros_sample: Vec::new() };
        vector.sample();
        Ok(vector)
    }

    /// Bits in the vector.
    #[must_use]
    pub fn len(&self) -> usize {
        self.len
    }

    /// Whether the vector has no bits at all.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.len == 0
    }

    /// Bits set.
    #[must_use]
    pub fn ones(&self) -> u64 {
        self.ones
    }

    /// Bits clear, within the length.
    #[must_use]
    pub fn zeros(&self) -> u64 {
        bits(self.len) - self.ones
    }

    /// Bits set strictly below `at`.
    #[must_use]
    pub fn rank1(&self, at: usize) -> u64 {
        if at >= self.len {
            return self.ones;
        }
        self.rank.rank(&self.words, at)
    }

    /// Bits clear strictly below `at`.
    #[must_use]
    pub fn rank0(&self, at: usize) -> u64 {
        let at = at.min(self.len);
        bits(at) - self.rank1(at)
    }

    /// Where the `nth` set bit is, counting from zero, or `None` if there are not that many.
    #[must_use]
    pub fn select1(&self, nth: u64) -> Option<usize> {
        if nth >= self.ones {
            return None;
        }
        Some(self.select(nth, true))
    }

    /// Where the `nth` clear bit is, counting from zero, or `None` if there are not that many.
    #[must_use]
    pub fn select0(&self, nth: u64) -> Option<usize> {
        if nth >= self.zeros() {
            return None;
        }
        Some(self.select(nth, false))
    }

    /// Bytes this costs on disk, which is the bitmap and the rank index and not the samples.
    #[must_use]
    pub fn bytes(&self) -> usize {
        self.words.len() * size_of::<u64>() + self.rank.bytes()
    }

    /// Appends the bitmap and its rank index.
    pub fn write(&self, out: &mut Vec<u8>) {
        for word in &self.words {
            out.extend_from_slice(&word.to_le_bytes());
        }
        self.rank.write(out);
    }

    /// Reads a vector of `len` bits from exactly the bytes [`BitVector::write`] produced.
    ///
    /// The rank index is read rather than rebuilt, and then it is not checked against the bitmap.
    /// Checking would be the pass that reading it was meant to avoid. A torn index is a wrong
    /// answer from a structure section 3.1 says can be deleted without changing any answer, so the
    /// protection that matters is the section checksum above this layer, not a recount here.
    ///
    /// # Errors
    ///
    /// If the bytes are not the length `len` implies, or if the tail past `len` is not zero.
    ///
    /// # Panics
    ///
    /// It does not. `chunks_exact` hands out eight bytes and the conversion to an eight byte array
    /// is the one the compiler cannot see through.
    pub fn read(bytes: &[u8], len: usize) -> Result<Self> {
        let words = len.div_ceil(64);
        let bitmap = words * size_of::<u64>();
        if bytes.len() < bitmap {
            return Err(malformed("a bit vector is shorter than its length implies"));
        }
        let held = bytes[..bitmap]
            .chunks_exact(size_of::<u64>())
            .map(|word| u64::from_le_bytes(word.try_into().expect("eight bytes")))
            .collect::<Vec<u64>>();
        let rank = Rank::read(&bytes[bitmap..], words)?;
        let tail = len % 64;
        if tail != 0 && held[len / 64] >> tail != 0 {
            return Err(malformed("a bit vector has bits set past its length"));
        }
        let ones = held.iter().map(|word| u64::from(word.count_ones())).sum();
        let mut vector =
            Self { words: held, len, ones, rank, ones_sample: Vec::new(), zeros_sample: Vec::new() };
        vector.sample();
        Ok(vector)
    }

    /// One superblock index per `SAMPLE` ones, and one per `SAMPLE` zeros.
    ///
    /// A pass over the superblock array and not over the bitmap, which is why this is cheap enough
    /// to do at read rather than store. The sample for group `k` is the answer a search for the
    /// `k * SAMPLE`th bit would give, so the window for any `nth` in that group is the sample for
    /// the group and the sample for the next one, and both counts being non-decreasing in the
    /// superblock index is what makes one forward walk enough to fill either array.
    fn sample(&mut self) {
        self.ones_sample = self.samples(true, self.ones);
        self.zeros_sample = self.samples(false, self.zeros());
    }

    fn samples(&self, set: bool, total: u64) -> Vec<u32> {
        let superblocks = self.rank.superblocks.len();
        let mut sample = Vec::with_capacity(usize::try_from(total.div_ceil(SAMPLE)).unwrap_or(0));
        let mut at = 0_usize;
        for group in 0..total.div_ceil(SAMPLE) {
            let target = group * SAMPLE;
            while at + 1 < superblocks && self.before(set, at + 1) <= target {
                at += 1;
            }
            sample.push(u32::try_from(at).unwrap_or(u32::MAX));
        }
        sample
    }

    /// Bits of the given value strictly before a superblock's first bit.
    fn before(&self, set: bool, superblock: usize) -> u64 {
        let ones = self.rank.ones_before_superblock(superblock);
        if set { ones } else { bits(superblock * SUPERBLOCK_BITS) - ones }
    }

    /// Where the `nth` bit of the given value is.
    ///
    /// Three narrowings, each over a structure the one above it points into: the sample picks a
    /// window of superblocks, a binary search picks the superblock, a walk of at most eight blocks
    /// picks the block, and a walk of at most eight words picks the word. The counts for zeros are
    /// the counts for ones subtracted from the position, which is why one routine answers both.
    fn select(&self, nth: u64, set: bool) -> usize {
        if self.rank.superblocks.is_empty() {
            return self.len;
        }
        let samples = if set { &self.ones_sample } else { &self.zeros_sample };
        let last = self.rank.superblocks.len() - 1;
        let group = usize::try_from(nth / SAMPLE).unwrap_or(usize::MAX);
        let from = samples.get(group).map_or(0, |at| *at as usize);
        let to = samples.get(group + 1).map_or(last, |at| *at as usize);
        let (mut low, mut high) = (from, to);
        while low < high {
            // The upper midpoint, because the search keeps the candidate rather than passing it,
            // and a lower midpoint with `low = middle` would not terminate.
            let middle = low + (high - low).div_ceil(2);
            if self.before(set, middle) <= nth {
                low = middle;
            } else {
                high = middle - 1;
            }
        }
        let superblock = low;
        let blocks = self.rank.blocks.len();
        let first = superblock * BLOCKS_PER_SUPERBLOCK;
        let within = |block: usize| -> u64 {
            let ones = self.rank.ones_before_block(block);
            if set { ones } else { bits(block * BLOCK_BITS) - ones }
        };
        let mut block = first;
        for candidate in first..(first + BLOCKS_PER_SUPERBLOCK).min(blocks) {
            if within(candidate) <= nth {
                block = candidate;
            } else {
                break;
            }
        }
        let mut before = within(block);
        for word in block * BLOCK_WORDS..self.words.len() {
            let held = if set { self.words[word] } else { !self.words[word] };
            let here = u64::from(held.count_ones());
            if before + here > nth {
                #[expect(
                    clippy::cast_possible_truncation,
                    reason = "a word holds at most 64 bits, so the offset within it fits a u32"
                )]
                let offset = (nth - before) as u32;
                return word * 64 + nth_set(held, offset) as usize;
            }
            before += here;
        }
        // Unreachable for an `nth` the callers checked against `ones` or `zeros`, and a saturating
        // answer rather than a panic if it ever is reached, because section 3.1 says this layer is
        // allowed to be slow and is not allowed to take the process down.
        self.len
    }
}

/// A bit count as a `u64`, which is what every count in here is compared against.
///
/// A position is a `usize` because it indexes a bitmap and a count is a `u64` because it is written
/// to a file, and the conversion between them is one place rather than nine.
fn bits(at: usize) -> u64 {
    u64::try_from(at).unwrap_or(u64::MAX)
}

/// Where the `nth` set bit of a word is, counting from zero.
///
/// The obvious loop, clearing the lowest set bit `nth` times. `pdep` does this in one instruction
/// on x86 and there is no portable way to say so yet, so this is the version that is correct
/// everywhere and the place to put the intrinsic when a measurement asks for it.
fn nth_set(mut word: u64, nth: u32) -> u32 {
    for _ in 0..nth {
        word &= word - 1;
    }
    word.trailing_zeros()
}

fn malformed(message: impl Into<String>) -> Error {
    Error::invalid_input(format!("invalid rudb bit vector: {}", message.into()))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Builds a vector from a list of bits, the slow and obviously correct way.
    fn vector(bits: &[bool]) -> BitVector {
        let mut words = vec![0_u64; bits.len().div_ceil(64)];
        for (at, bit) in bits.iter().enumerate() {
            if *bit {
                words[at / 64] |= 1 << (at % 64);
            }
        }
        BitVector::new(words, bits.len()).expect("build")
    }

    /// Checks rank and select against counting by hand, at every position.
    fn agrees(bits: &[bool]) {
        let built = vector(bits);
        let (mut ones, mut zeros) = (Vec::new(), Vec::new());
        for (at, bit) in bits.iter().enumerate() {
            assert_eq!(built.rank1(at), ones.len() as u64, "rank1 at {at}");
            assert_eq!(built.rank0(at), zeros.len() as u64, "rank0 at {at}");
            if *bit { ones.push(at) } else { zeros.push(at) }
        }
        assert_eq!(built.ones(), ones.len() as u64);
        assert_eq!(built.zeros(), zeros.len() as u64);
        for (nth, at) in ones.iter().enumerate() {
            assert_eq!(built.select1(nth as u64), Some(*at), "select1 of {nth}");
        }
        for (nth, at) in zeros.iter().enumerate() {
            assert_eq!(built.select0(nth as u64), Some(*at), "select0 of {nth}");
        }
        assert_eq!(built.select1(ones.len() as u64), None, "there is no one past the last");
        assert_eq!(built.select0(zeros.len() as u64), None, "there is no zero past the last");
    }

    #[test]
    fn an_empty_vector_answers_nothing_rather_than_panicking() {
        let built = vector(&[]);
        assert!(built.is_empty());
        assert_eq!(built.ones(), 0);
        assert_eq!(built.zeros(), 0);
        assert_eq!(built.select1(0), None);
        assert_eq!(built.select0(0), None);
        assert_eq!(built.rank1(0), 0);
        assert_eq!(built.rank0(9), 0);
    }

    #[test]
    fn a_vector_of_one_bit_each_way_agrees_with_counting() {
        agrees(&[true]);
        agrees(&[false]);
    }

    #[test]
    fn alternating_bits_agree_with_counting_across_a_word_boundary() {
        agrees(&(0..200).map(|at| at % 2 == 0).collect::<Vec<bool>>());
    }

    #[test]
    fn a_vector_longer_than_a_superblock_agrees_with_counting() {
        // 4096 bits is one superblock and eight blocks, so this is the first size where every level
        // of the index is exercised and the sample has more than one entry to choose between.
        agrees(&(0..10_000).map(|at| at % 7 == 0).collect::<Vec<bool>>());
    }

    #[test]
    fn a_vector_that_is_almost_all_ones_agrees_with_counting() {
        // The sparse direction of each search. With one zero every thousand bits, a select0 sample
        // spans many superblocks, which is the case the binary search inside the window exists for.
        agrees(&(0..20_000).map(|at| at % 1000 != 0).collect::<Vec<bool>>());
    }

    #[test]
    fn a_vector_that_is_almost_all_zeros_agrees_with_counting() {
        agrees(&(0..20_000).map(|at| at % 1000 == 0).collect::<Vec<bool>>());
    }

    #[test]
    fn a_vector_of_all_ones_and_one_of_all_zeros_both_agree() {
        agrees(&vec![true; 5000]);
        agrees(&vec![false; 5000]);
    }

    #[test]
    fn a_run_of_ones_longer_than_the_select_sample_is_found() {
        // Section 3.4's monotone link puts one run of ones per parent, and a parent with more than
        // four thousand ninety six children puts a whole sample group inside one run.
        let mut bits = vec![false; 3];
        bits.extend(std::iter::repeat_n(true, 9000));
        bits.push(false);
        agrees(&bits);
    }

    #[test]
    fn rank_past_the_end_saturates_rather_than_reading_past_it() {
        let built = vector(&[true, false, true]);
        assert_eq!(built.rank1(3), 2);
        assert_eq!(built.rank1(9999), 2);
        assert_eq!(built.rank0(9999), 1);
    }

    #[test]
    fn a_vector_survives_being_written_and_read_back() {
        let bits = (0..5000).map(|at| at % 3 == 0).collect::<Vec<bool>>();
        let built = vector(&bits);
        let mut bytes = Vec::new();
        built.write(&mut bytes);
        assert_eq!(bytes.len(), built.bytes(), "bytes() is what write() writes");
        let read = BitVector::read(&bytes, bits.len()).expect("read");
        assert_eq!(read.ones(), built.ones());
        for nth in 0..read.ones() {
            assert_eq!(read.select1(nth), built.select1(nth));
        }
        for nth in 0..read.zeros() {
            assert_eq!(read.select0(nth), built.select0(nth));
        }
    }

    #[test]
    fn a_word_count_that_does_not_match_the_length_is_refused() {
        assert!(BitVector::new(vec![0; 2], 64).is_err());
        assert!(BitVector::new(vec![0; 1], 65).is_err());
    }

    #[test]
    fn a_bit_set_past_the_length_is_refused_rather_than_counted() {
        // It would be counted by rank and found by select, so a vector that accepted it would give
        // answers about a bit nobody wrote.
        assert!(BitVector::new(vec![1 << 40], 8).is_err());
        let mut bytes = Vec::new();
        vector(&[true, false, true]).write(&mut bytes);
        bytes[0] |= 1 << 4;
        assert!(BitVector::read(&bytes, 3).is_err());
    }

    #[test]
    fn a_truncated_vector_is_refused_rather_than_read_past() {
        let mut bytes = Vec::new();
        vector(&(0..5000).map(|at| at % 3 == 0).collect::<Vec<bool>>()).write(&mut bytes);
        assert!(BitVector::read(&bytes[..bytes.len() - 1], 5000).is_err());
        assert!(BitVector::read(&bytes[..4], 5000).is_err());
    }

    #[test]
    fn the_rank_index_costs_about_an_eighth_of_the_bitmap() {
        // Section 3.4 budgets the whole select structure at about thirteen percent. The samples are
        // not stored, so what a file pays is this.
        let built = vector(&(0..1_000_000).map(|at| at % 5 == 0).collect::<Vec<bool>>());
        let bitmap = 1_000_000 / 8;
        assert!(built.bytes() > bitmap, "{} is not more than {bitmap}", built.bytes());
        assert!(
            built.bytes() < bitmap * 6 / 5,
            "{} is more than a fifth over {bitmap}",
            built.bytes()
        );
    }
}
