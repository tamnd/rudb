//! A list that grows a block at a time and never moves what it already holds.
//!
//! A radix exchange scatters every row of its input into one run per partition and instance before
//! anything reads them back, and it reads them back once, in order. A `Vec` is the wrong shape for
//! that: each time a run fills it is copied into one twice the size and the old one is freed, and
//! with every run of an instance filling at about the same rate the freed buffers are all one size
//! at once and nothing asks for that size again. On ClickBench q33, ten million sixteen byte records
//! across 64 runs, that left 260 MB resident on one thread for 160 MB of records. Here a full block
//! stays where it is and the next one is added beside it, so the records are copied once, into the
//! run, and what is resident is the records and at most the unwritten end of one block a run.
//!
//! Every block holds a power of two values, so where a position lies is a few shifts away and a run
//! can be read and rewritten in place by position, which is what the encoded count's compaction
//! does to its runs.

use std::mem::size_of;

/// How many values the first block of a list holds. Small, because most runs of a small input stay
/// small.
const FIRST: usize = 128;

/// How many values the largest block holds. Each block is twice the one before up to this, so a run
/// of a few records costs a few kilobytes and a long one is a list of these. The sizes are counted
/// in values rather than bytes so that two lists of different types pushed in step, a run's records
/// and their weights, put a position in the same block and can be walked with one cursor.
const LARGEST: usize = 8_192;

/// How many bits a position within the largest block takes, so that a block and a position in it
/// pack into one integer as `block << WITHIN_BITS | within`.
pub(crate) const WITHIN_BITS: u32 = LARGEST.ilog2();

/// How many blocks double before they reach the largest size.
const STEPS: usize = (LARGEST / FIRST).ilog2() as usize;

/// How many values the doubling blocks hold between them.
const RAMP: usize = LARGEST - FIRST;

/// How many values block `block` holds.
#[inline]
pub(crate) fn size(block: usize) -> usize {
    if block < STEPS { FIRST << block } else { LARGEST }
}

/// The first position block `block` holds.
pub(crate) fn start(block: usize) -> usize {
    if block < STEPS { FIRST * ((1 << block) - 1) } else { RAMP + (block - STEPS) * LARGEST }
}

/// The block position `at` lies in and where in it.
#[inline]
pub(crate) fn locate(at: usize) -> (usize, usize) {
    if at < RAMP {
        let block = (at / FIRST + 1).ilog2() as usize;
        (block, at - FIRST * ((1 << block) - 1))
    } else {
        let past = at - RAMP;
        (STEPS + past / LARGEST, past % LARGEST)
    }
}

#[derive(Debug)]
pub(crate) struct Blocks<T> {
    /// The blocks that are full, in the order they were filled.
    full: Vec<Vec<T>>,
    /// How many values the full blocks hold between them.
    held: usize,
    /// The block being written, which never grows past the capacity it was made with.
    tail: Vec<T>,
}

impl<T> Default for Blocks<T> {
    fn default() -> Self {
        Self { full: Vec::new(), held: 0, tail: Vec::new() }
    }
}

impl<T: Copy> Blocks<T> {
    #[inline]
    pub(crate) fn push(&mut self, value: T) {
        if self.tail.len() == self.tail.capacity() {
            self.grow();
        }
        self.tail.push(value);
    }

    #[cold]
    fn grow(&mut self) {
        // The tail is only empty before the first push, and that is the one time it is not a block.
        if self.tail.capacity() != 0 {
            let full = std::mem::take(&mut self.tail);
            self.held += full.len();
            self.full.push(full);
        }
        self.tail = Vec::with_capacity(size(self.full.len()));
    }

    /// The blocks themselves, in order, so a caller reading the values once can let each block go
    /// as soon as it is through with it.
    pub(crate) fn into_blocks(self) -> impl Iterator<Item = Vec<T>> {
        self.full.into_iter().chain(std::iter::once(self.tail))
    }

    pub(crate) fn len(&self) -> usize {
        self.held + self.tail.len()
    }

    pub(crate) fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// The value at `within` of block `block`, for a caller walking a position it already located.
    #[inline]
    pub(crate) fn slot(&self, block: usize, within: usize) -> &T {
        if block < self.full.len() { &self.full[block][within] } else { &self.tail[within] }
    }

    #[inline]
    pub(crate) fn slot_mut(&mut self, block: usize, within: usize) -> &mut T {
        if block < self.full.len() { &mut self.full[block][within] } else { &mut self.tail[within] }
    }

    /// The values in the order they were pushed, a block at a time.
    pub(crate) fn slices(&self) -> impl Iterator<Item = &[T]> {
        self.full.iter().map(Vec::as_slice).chain(std::iter::once(self.tail.as_slice()))
    }

    /// The bytes the blocks take, written or not.
    pub(crate) fn footprint(&self) -> usize {
        let blocks: usize = self.full.iter().map(Vec::capacity).sum();
        (blocks + self.tail.capacity()) * size_of::<T>()
            + self.full.capacity() * size_of::<Vec<T>>()
    }
}

impl<T: Copy> FromIterator<T> for Blocks<T> {
    fn from_iter<I: IntoIterator<Item = T>>(values: I) -> Self {
        let mut blocks = Self::default();
        for value in values {
            blocks.push(value);
        }
        blocks
    }
}

#[cfg(test)]
mod tests {
    use super::Blocks;

    #[test]
    fn blocks_give_back_what_was_pushed_in_order() {
        let mut blocks = Blocks::default();
        assert!(blocks.is_empty());
        for value in 0..200_000_u64 {
            blocks.push(value);
        }
        assert_eq!(blocks.len(), 200_000);
        let read: Vec<u64> = blocks.slices().flatten().copied().collect();
        assert_eq!(read, (0..200_000).collect::<Vec<_>>());
        // No block is larger than the cap, so a long list is many blocks and none was copied.
        assert!(blocks.slices().all(|slice| slice.len() <= super::LARGEST));
        assert!(blocks.footprint() >= 200_000 * 8);
        assert!(blocks.footprint() < 200_000 * 8 + super::LARGEST * 8 + 4096);
    }

    #[test]
    fn blocks_are_read_and_written_by_place() {
        let mut blocks: Blocks<[u64; 3]> = Blocks::default();
        for value in 0..100_000_u64 {
            blocks.push([value, 0, 0]);
        }
        for at in [0, 1, 127, 128, 383, 384, 1_000, 8_063, 8_064, 20_000, 99_999] {
            let (block, within) = super::locate(at);
            assert_eq!(super::start(block) + within, at);
            assert!(within < super::size(block) && within < 1 << super::WITHIN_BITS);
            assert_eq!(blocks.slot(block, within)[0], at as u64);
            blocks.slot_mut(block, within)[1] = 1;
        }
        let read: Vec<[u64; 3]> = blocks.into_blocks().flatten().collect();
        assert_eq!(read.len(), 100_000);
        assert!(read.iter().enumerate().all(|(at, value)| value[0] == at as u64));
        assert_eq!(read.iter().filter(|value| value[1] == 1).count(), 11);
    }
}
