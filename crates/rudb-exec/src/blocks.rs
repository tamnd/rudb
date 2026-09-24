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

use std::mem::size_of;

/// The first block a list takes, in bytes. Small, because most runs of a small input stay small.
const FIRST_BYTES: usize = 4 << 10;

/// The largest block a list takes, in bytes. Each block is twice the one before up to this, so a
/// run of a few records costs a few kilobytes and a long one is a list of these.
const LARGEST_BYTES: usize = 256 << 10;

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
        let first = (FIRST_BYTES / size_of::<T>().max(1)).max(1);
        let largest = (LARGEST_BYTES / size_of::<T>().max(1)).max(first);
        let next = self.tail.capacity().saturating_mul(2).clamp(first, largest);
        let full = std::mem::replace(&mut self.tail, Vec::with_capacity(next));
        if !full.is_empty() {
            self.held += full.len();
            self.full.push(full);
        }
    }

    pub(crate) fn len(&self) -> usize {
        self.held + self.tail.len()
    }

    pub(crate) fn is_empty(&self) -> bool {
        self.len() == 0
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
        assert!(blocks.slices().all(|slice| slice.len() <= (256 << 10) / 8));
        assert!(blocks.footprint() >= 200_000 * 8);
        assert!(blocks.footprint() < 200_000 * 8 + (256 << 10) + 4096);
    }
}
