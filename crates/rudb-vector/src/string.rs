//! The string representation.
//!
//! `spec/07-execution.md` section 7.1: a string is a 16 byte structure, 4 bytes of length, 4 bytes
//! of prefix, and 8 bytes that are either the rest of a short string or a way to find a long one.
//! Strings of 12 bytes or fewer live entirely inside the structure. The prefix means most
//! comparisons and most equality tests answer without dereferencing anything, which on the string
//! heavy queries in ClickBench is the difference between a cache hit and a cache miss per row.
//!
//! **Where this differs from the specification, and why.** The document says the last 8 bytes are
//! a pointer, which is what DuckDB and Umbra do. Here they are a block index and an offset, which
//! is what Arrow's `StringView` does. The sizes are identical, the prefix trick is identical, and
//! the prefix trick is the part that makes it fast. The difference is one predictable load against
//! one pointer chase on the slow path only, and in exchange the whole representation is safe code
//! with no pinning machinery, which does not exist until the buffer manager arrives at M2. This is
//! the kind of decision that gets remeasured rather than argued about, and it is tracked as an
//! issue so that M3 measures it instead of inheriting it.

/// The longest string that fits entirely inside a view.
pub const INLINE_LIMIT: usize = 12;

/// A 16 byte handle on a string.
///
/// The layout is a `u32` length and 12 bytes of payload. For a string of 12 bytes or fewer the
/// payload is the string, zero padded. For a longer one the first 4 bytes are the prefix, the next
/// 4 are the index of the block holding it, and the last 4 are the offset into that block.
///
/// A view on its own cannot produce a long string, only a short one. That is deliberate: the
/// blocks live in the [`StringColumn`] and the borrow checker is what stops a view from outliving
/// them, rather than a rule somebody has to remember.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct StringView {
    length: u32,
    payload: [u8; 12],
}

impl StringView {
    /// A view on a string that fits inline.
    ///
    /// # Panics
    ///
    /// If the string is longer than [`INLINE_LIMIT`]. Callers that do not know the length go
    /// through [`StringColumn::push`], which decides.
    #[must_use]
    pub fn inline(text: &str) -> Self {
        assert!(text.len() <= INLINE_LIMIT, "a string of {} bytes is not inline", text.len());
        let mut payload = [0u8; 12];
        payload[..text.len()].copy_from_slice(text.as_bytes());
        Self { length: text.len() as u32, payload }
    }

    /// A view on a string that lives in a block.
    fn indirect(text: &str, block: u32, offset: u32) -> Self {
        let mut payload = [0u8; 12];
        payload[..4].copy_from_slice(&text.as_bytes()[..4]);
        payload[4..8].copy_from_slice(&block.to_le_bytes());
        payload[8..].copy_from_slice(&offset.to_le_bytes());
        Self { length: text.len() as u32, payload }
    }

    /// The length in bytes.
    #[must_use]
    pub fn len(&self) -> usize {
        self.length as usize
    }

    /// Whether the string is empty.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.length == 0
    }

    /// Whether the whole string is in the view.
    #[must_use]
    pub fn is_inline(&self) -> bool {
        self.len() <= INLINE_LIMIT
    }

    /// The first four bytes, zero padded.
    ///
    /// This is the whole point of the representation. Two strings with different prefixes are
    /// different, and two strings with the same prefix are usually equal, so a filter on a string
    /// column resolves without touching the payload on almost every row.
    #[must_use]
    pub fn prefix(&self) -> [u8; 4] {
        [self.payload[0], self.payload[1], self.payload[2], self.payload[3]]
    }

    /// The bytes, when the whole string is in the view.
    ///
    /// A comparison wants bytes rather than a `&str`, because SQL's string order is byte order and
    /// because [`Self::as_inline_str`] pays for a UTF-8 validation that a comparison has no use
    /// for. On a filter against a varchar column that validation is the whole cost of the row.
    #[must_use]
    pub fn inline_bytes(&self) -> Option<&[u8]> {
        if self.is_inline() { Some(&self.payload[..self.len()]) } else { None }
    }

    /// The string, when it is short enough to be in the view.
    #[must_use]
    pub fn as_inline_str(&self) -> Option<&str> {
        if !self.is_inline() {
            return None;
        }
        // Every constructor takes a `&str`, so the bytes came from valid UTF-8 and a prefix of the
        // inline payload up to the recorded length is exactly what was written.
        std::str::from_utf8(&self.payload[..self.len()]).ok()
    }

    fn block(&self) -> usize {
        u32::from_le_bytes([self.payload[4], self.payload[5], self.payload[6], self.payload[7]])
            as usize
    }

    fn offset(&self) -> usize {
        u32::from_le_bytes([self.payload[8], self.payload[9], self.payload[10], self.payload[11]])
            as usize
    }

    /// Whether these two views are definitely different, answered from the view alone.
    ///
    /// A `false` here means the payloads have to be compared. A `true` means they do not, which on
    /// a filter against a selective literal is almost every row.
    #[must_use]
    pub fn definitely_differs(&self, other: &Self) -> bool {
        self.length != other.length || self.prefix() != other.prefix()
    }
}

/// How much string data one block holds before another is started.
///
/// 16 KiB is four pages. Small enough that a column of short strings does not round up to
/// something silly, large enough that the per-block bookkeeping disappears.
const BLOCK_SIZE: usize = 16 * 1024;

/// A column of strings: the views, and the blocks the long ones live in.
///
/// Blocks are append only and never move, so an offset recorded in a view stays correct for the
/// life of the column. Pushing a string longer than a block gives it a block of its own rather
/// than splitting it, which keeps every string contiguous and keeps [`Self::get`] free of a
/// stitching path that would be wrong more often than it ran.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct StringColumn {
    views: Vec<StringView>,
    blocks: Vec<Vec<u8>>,
}

impl StringColumn {
    /// An empty column.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// An empty column with room for `capacity` strings.
    #[must_use]
    pub fn with_capacity(capacity: usize) -> Self {
        Self { views: Vec::with_capacity(capacity), blocks: Vec::new() }
    }

    /// How many strings are in the column.
    #[must_use]
    pub fn len(&self) -> usize {
        self.views.len()
    }

    /// Whether the column has no strings in it.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.views.is_empty()
    }

    /// The views, for a kernel that wants to compare prefixes without reading any payload.
    #[must_use]
    pub fn views(&self) -> &[StringView] {
        &self.views
    }

    /// Appends a string and returns its index.
    pub fn push(&mut self, text: &str) -> usize {
        let view = if text.len() <= INLINE_LIMIT {
            StringView::inline(text)
        } else {
            let (block, offset) = self.append_bytes(text.as_bytes());
            StringView::indirect(text, block, offset)
        };
        self.views.push(view);
        self.views.len() - 1
    }

    /// The bytes at `index`, or `None` past the end.
    ///
    /// This is what a comparison, a hash and an equality check all actually want, and it is worth
    /// having separately from [`Self::get`] because that one validates UTF-8 and they do not need
    /// it. Everything in a column arrived through [`Self::push`], which takes a `&str`, so the
    /// bytes are valid either way and the validation is a scan of the payload that changes no
    /// answer. On a varchar filter it was measured at most of the per row cost.
    #[must_use]
    pub fn bytes(&self, index: usize) -> Option<&[u8]> {
        let view = self.views.get(index)?;
        if let Some(inline) = view.inline_bytes() {
            return Some(inline);
        }
        let block = self.blocks.get(view.block())?;
        block.get(view.offset()..view.offset() + view.len())
    }

    /// The string at `index`, or `None` past the end.
    #[must_use]
    pub fn get(&self, index: usize) -> Option<&str> {
        // Written from a `&str` into a block that is append only, so the bytes are the same bytes.
        std::str::from_utf8(self.bytes(index)?).ok()
    }

    /// Every string in order.
    pub fn iter(&self) -> impl Iterator<Item = &str> {
        (0..self.len()).filter_map(|index| self.get(index))
    }

    /// Total bytes of payload held in blocks, which is what the memory accounting wants.
    #[must_use]
    pub fn heap_bytes(&self) -> usize {
        self.blocks.iter().map(Vec::len).sum()
    }

    fn append_bytes(&mut self, bytes: &[u8]) -> (u32, u32) {
        let fits = self
            .blocks
            .last()
            .is_some_and(|block| block.len() + bytes.len() <= block.capacity().max(BLOCK_SIZE));
        if !fits {
            self.blocks.push(Vec::with_capacity(BLOCK_SIZE.max(bytes.len())));
        }
        let block_index = self.blocks.len() - 1;
        let block = &mut self.blocks[block_index];
        let offset = block.len();
        block.extend_from_slice(bytes);
        // A column with more than 4 billion blocks or a block over 4 GiB is not a thing that can
        // exist here, since a vector holds 1024 values and a row group holds 122,880.
        (block_index as u32, offset as u32)
    }
}

impl<'a> Extend<&'a str> for StringColumn {
    fn extend<T: IntoIterator<Item = &'a str>>(&mut self, iter: T) {
        for text in iter {
            self.push(text);
        }
    }
}

impl<'a> FromIterator<&'a str> for StringColumn {
    fn from_iter<T: IntoIterator<Item = &'a str>>(iter: T) -> Self {
        let mut column = Self::new();
        column.extend(iter);
        column
    }
}

#[cfg(test)]
mod tests {
    use super::{INLINE_LIMIT, StringColumn, StringView};

    #[test]
    fn a_view_is_sixteen_bytes_and_stays_sixteen_bytes() {
        // The number the whole design is built around. A vector of 1024 strings is 16 KiB of
        // views, which is the budget spec/07-execution.md section 7.1 spends on purpose.
        assert_eq!(size_of::<StringView>(), 16);
        assert_eq!(align_of::<StringView>(), 4);
    }

    #[test]
    fn twelve_bytes_is_inline_and_thirteen_is_not() {
        let mut column = StringColumn::new();
        column.push("123456789012");
        column.push("1234567890123");
        assert!(column.views()[0].is_inline());
        assert!(!column.views()[1].is_inline());
        assert_eq!(column.get(0), Some("123456789012"));
        assert_eq!(column.get(1), Some("1234567890123"));
        assert_eq!(INLINE_LIMIT, 12);
    }

    #[test]
    fn a_prefix_answers_the_comparison_without_reading_the_payload() {
        let mut column = StringColumn::new();
        column.push("https://example.com/a");
        column.push("https://example.com/b");
        column.push("mailto:someone@example.com");
        let views = column.views();
        // Same prefix, same length: the payloads have to be read. This is the case the prefix
        // cannot help with, and on a URL column it is the common case, which is why the
        // dictionary work at M3 matters more than this does.
        assert!(!views[0].definitely_differs(&views[1]));
        // Different prefix: answered from the view.
        assert!(views[0].definitely_differs(&views[2]));
    }

    #[test]
    fn a_string_longer_than_a_block_gets_a_block_of_its_own() {
        let long = "x".repeat(40 * 1024);
        let mut column = StringColumn::new();
        column.push("short");
        column.push(&long);
        column.push("also short");
        assert_eq!(column.get(1), Some(long.as_str()));
        assert_eq!(column.get(2), Some("also short"));
        assert_eq!(column.heap_bytes(), long.len());
    }

    #[test]
    fn blocks_hold_many_strings_and_the_offsets_stay_right() {
        let mut column = StringColumn::new();
        let strings: Vec<String> =
            (0..2000).map(|i| format!("value number {i} padded out")).collect();
        for text in &strings {
            column.push(text);
        }
        for (index, text) in strings.iter().enumerate() {
            assert_eq!(column.get(index), Some(text.as_str()), "at {index}");
        }
        assert_eq!(column.len(), 2000);
        assert_eq!(column.iter().count(), 2000);
    }

    #[test]
    fn the_empty_string_is_inline_and_reads_back_empty() {
        let mut column = StringColumn::new();
        column.push("");
        assert_eq!(column.get(0), Some(""));
        assert!(column.views()[0].is_empty());
        assert_eq!(column.heap_bytes(), 0);
    }

    #[test]
    fn multibyte_text_survives_the_inline_boundary() {
        // The boundary is bytes and not characters, so a four byte emoji is what decides whether
        // a three character string is inline.
        let mut column = StringColumn::new();
        column.push("héllo wörld");
        column.push("🦀🦀🦀🦀");
        assert_eq!(column.get(0), Some("héllo wörld"));
        assert_eq!(column.get(1), Some("🦀🦀🦀🦀"));
        assert!(!column.views()[1].is_inline());
    }

    #[test]
    fn reading_past_the_end_is_none_rather_than_a_panic() {
        let column: StringColumn = ["a", "b"].into_iter().collect();
        assert_eq!(column.get(2), None);
        assert_eq!(column.len(), 2);
    }

    /// The bytes and the string have to be the same string on both sides of the inline boundary
    /// and on multibyte text, because the comparison kernels read the bytes and everything else
    /// reads the string, and a disagreement between them would be a filter that matched a row the
    /// projection then printed differently.
    #[test]
    fn the_bytes_and_the_string_are_the_same_string() {
        let long = "x".repeat(9000);
        let words = ["", "a", "twelve bytes", "thirteen bytes", "π is two bytes", &long];
        let column: StringColumn = words.into_iter().collect();
        for (index, text) in words.iter().enumerate() {
            assert_eq!(column.bytes(index), Some(text.as_bytes()), "at {index}");
            assert_eq!(column.get(index), Some(*text), "at {index}");
        }
        assert_eq!(column.bytes(words.len()), None);
    }
}
