//! A unit of work handed out by a source.

use std::fmt;

/// One unit of scan work.
///
/// A morsel is the granule the scheduler hands to a thread. It is deliberately larger than a
/// chunk, because a block of a wide string column does not fit in a chunk and because forcing the
/// two to be equal would make the storage format's block size and the execution engine's cache
/// working set the same decision, which they are not.
///
/// `cursor` is the source's own position within the morsel and it is the reason
/// [`Source::read`](crate::Source::read) can be called many times for one morsel. The source owns
/// what the numbers mean. For a scan they are row positions in the table. For an in memory table
/// they are row positions in the vector. A source that needs richer per morsel state than three
/// integers keeps it in its own structure keyed by [`Morsel::index`], and if that turns out to be
/// the common case rather than the rare one then this type grows an opaque payload and the change
/// is one file.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Morsel {
    index: u64,
    start: u64,
    end: u64,
    cursor: u64,
}

impl Morsel {
    /// A morsel covering `start .. end`, positioned at the beginning.
    ///
    /// # Panics
    ///
    /// When `end` is before `start`, which is a mistake in a source rather than anything a query
    /// can cause.
    #[must_use]
    pub fn new(index: u64, start: u64, end: u64) -> Self {
        assert!(start <= end, "morsel {index} covers {start} to {end}");
        Self { index, start, end, cursor: start }
    }

    /// Which unit of work this is. Sources number their own and the numbers appear in metrics.
    #[must_use]
    pub const fn index(self) -> u64 {
        self.index
    }

    /// The first position the morsel covers.
    #[must_use]
    pub const fn start(self) -> u64 {
        self.start
    }

    /// One past the last position the morsel covers.
    #[must_use]
    pub const fn end(self) -> u64 {
        self.end
    }

    /// Where the source has got to.
    #[must_use]
    pub const fn cursor(self) -> u64 {
        self.cursor
    }

    /// How much is left.
    #[must_use]
    pub const fn remaining(self) -> u64 {
        self.end.saturating_sub(self.cursor)
    }

    /// How much the morsel covers in total.
    #[must_use]
    pub const fn len(self) -> u64 {
        self.end.saturating_sub(self.start)
    }

    /// Whether the morsel covers nothing, which a source is allowed to hand out and a driver has
    /// to survive.
    #[must_use]
    pub const fn is_empty(self) -> bool {
        self.len() == 0
    }

    /// Whether the source has read all of it.
    #[must_use]
    pub const fn is_drained(self) -> bool {
        self.cursor >= self.end
    }

    /// Move the cursor on by `rows`, stopping at the end rather than running past it.
    pub fn advance(&mut self, rows: u64) {
        self.cursor = self.end.min(self.cursor.saturating_add(rows));
    }
}

impl fmt::Display for Morsel {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "morsel {} covering {} to {} at {}",
            self.index, self.start, self.end, self.cursor
        )
    }
}
