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

use rudb_common::{Error, Result};

use crate::buffer::Buffer;

/// The longest string that fits entirely inside a view.
pub const INLINE_LIMIT: usize = 12;

/// A 16 byte handle on a string.
///
/// The layout is a `u32` length and 12 bytes of payload. For a string of 12 bytes or fewer the
/// payload is the string, zero padded. For a longer one the first 4 bytes are the prefix and the
/// last 8 are the offset into the column's arena.
///
/// Arrow spends 4 of those 8 bytes on a buffer index and 4 on an offset within the buffer, because
/// an Arrow array is a list of buffers. This column is one arena, so there is no buffer to name and
/// the whole 8 bytes are the offset, which reads as one load rather than two and takes the reachable
/// size of a column from 4 GiB to more than anything will ever put in one.
///
/// A view on its own cannot produce a long string, only a short one. That is deliberate: the arena
/// lives in the [`StringColumn`] and the borrow checker is what stops a view from outliving it,
/// rather than a rule somebody has to remember.
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

    /// A view on a string that lives in the arena.
    fn indirect(text: &str, offset: u64) -> Self {
        let mut payload = [0u8; 12];
        payload[..4].copy_from_slice(&text.as_bytes()[..4]);
        payload[4..].copy_from_slice(&offset.to_le_bytes());
        Self { length: text.len() as u32, payload }
    }

    /// A view on bytes, whatever they are, wherever they turn out to live.
    ///
    /// The one constructor that takes bytes rather than a `&str`, and the two callers want it for
    /// different reasons. A copy between two columns has bytes that were validated on the way into
    /// the first one and validating again would be work for nothing. A `BLOB` has bytes that were
    /// never text and are not going to become it. `offset` is where they are in the destination
    /// arena and is ignored for a string short enough to sit in the view.
    fn over(bytes: &[u8], offset: u64) -> Self {
        let mut payload = [0u8; 12];
        if bytes.len() <= INLINE_LIMIT {
            payload[..bytes.len()].copy_from_slice(bytes);
        } else {
            payload[..4].copy_from_slice(&bytes[..4]);
            payload[4..].copy_from_slice(&offset.to_le_bytes());
        }
        Self { length: bytes.len() as u32, payload }
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
        // `None` rather than a panic for a view that holds a blob, since the payload is whatever
        // was written and only a column of text can promise that is a string.
        std::str::from_utf8(&self.payload[..self.len()]).ok()
    }

    fn offset(&self) -> usize {
        u64::from_le_bytes([
            self.payload[4],
            self.payload[5],
            self.payload[6],
            self.payload[7],
            self.payload[8],
            self.payload[9],
            self.payload[10],
            self.payload[11],
        ]) as usize
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

/// A column of strings: the views, and the one arena the long ones live in.
///
/// The arena is append only, so an offset recorded in a view stays correct for the life of the
/// column even though the arena's address does not. That is the property a `Vec<u8>` has and a raw
/// pointer into it does not, and it is the reason a view holds an offset.
///
/// This was a `Vec<Vec<u8>>` of fixed size blocks, which meant reading one long string was two
/// dependent loads, the outer vector's element to find the block's data pointer and then the bytes.
/// One arena makes it one, from a base the compiler can keep in a register across a row loop, and it
/// deletes the case where a string longer than a block needed a block of its own. On server3, over a
/// chunk of 1024 strings, comparing a column against a literal went from 14.9 nanoseconds a row to
/// 13.2 at 40 bytes a string and from 14.2 to 12.9 at 120, gathering half the rows from 29.5 to 25.3
/// and from 36.9 to 29.1, and building the column from 12.0 to 8.9 at 40 bytes.
///
/// # The one number that got worse, and what it actually is
///
/// Building a column whose payload passes 128 KiB, which at 1024 rows means strings averaging more
/// than 128 bytes, went the other way: 14.6 nanoseconds a row to 41.0. That is not the copy and it
/// is not the doubling, it is glibc. An allocation that size comes from `mmap` rather than the heap,
/// so it is handed back to the kernel when the column is dropped and the next chunk faults every
/// page of it in again, while sixteen KiB blocks come back off a free list already faulted. Run the
/// same benchmark with `MALLOC_MMAP_THRESHOLD_` raised and the arena builds that column in 9.6
/// nanoseconds a row against the blocks' 16.2, so the design is not what is slow there.
///
/// The fix is that a chunk's payload should come from a pool the engine owns rather than from
/// `malloc` per chunk, which is the buffer manager at layer three and is where this belongs.
/// [`Self::reserve_bytes`] is the part that is available now, and it recovers a quarter of it.
///
/// # Equality is about the strings and not about the arena
///
/// [`Self::over`] means two columns holding exactly the same strings can hold completely different
/// arenas, because one of them was built by copying the strings in and the other was built over a
/// page that already had them somewhere in it with other strings in between. Derived equality would
/// call those two columns different, and every test in the workspace that compares two vectors would
/// then be asserting on how a column was built rather than on what is in it. So equality is the
/// strings, position by position, which is the only definition that survives the seam.
#[derive(Debug, Clone, Default, Eq)]
pub struct StringColumn {
    views: Vec<StringView>,
    arena: Buffer<u8>,
}

impl StringColumn {
    /// How many bytes of memory this column is holding.
    ///
    /// The views and the arena. A short string lives inside its view and costs nothing beyond it,
    /// which is the whole reason the representation exists, so a column of short strings costs
    /// sixteen bytes a string and a column of long ones costs sixteen plus the bytes themselves.
    #[must_use]
    pub fn footprint(&self) -> usize {
        self.views.capacity() * size_of::<StringView>() + self.arena.footprint()
    }

    /// An empty column.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// An empty column with room for `capacity` strings.
    #[must_use]
    pub fn with_capacity(capacity: usize) -> Self {
        Self { views: Vec::with_capacity(capacity), arena: Buffer::new() }
    }

    /// A column with no strings in it yet, over an arena that already holds bytes.
    ///
    /// The seam `spec/engine/03-data-plane.md` section 3.5 asks for. Without it the only way in is
    /// [`Self::push`], which copies, so a scan reading a Parquet page of strings copies every byte of
    /// the page into an arena and the query then reads the copy. With it the page is the arena: the
    /// scan hands the bytes over once, records where each string starts with
    /// [`Self::push_in_place`], and nothing is copied but the views.
    ///
    /// It is useful today, because a reader that already has the page in a `Vec<u8>` can move it in
    /// rather than copy out of it. It matters at layer three, when the [`Buffer`] is the pinned page
    /// itself and the move is not even that.
    ///
    /// Appending with [`Self::push`] afterwards still works and still appends to the arena. That is
    /// the case to keep away from once a real page is in here, because writing through a borrowed
    /// buffer copies it, which is [`Buffer::to_mut`] and is the whole page.
    #[must_use]
    pub fn over(arena: Buffer<u8>) -> Self {
        Self { views: Vec::new(), arena }
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
            let offset = self.arena.len() as u64;
            self.arena.extend_from_slice(text.as_bytes());
            StringView::indirect(text, offset)
        };
        self.views.push(view);
        self.views.len() - 1
    }

    /// Appends the string at `index` of another column, and returns its index here.
    ///
    /// This is what a gather and a slice over a string column want, and it is worth having next to
    /// [`Self::push`] because that one takes a `&str` and the only way to get one out of a column
    /// is [`Self::get`], which validates UTF-8. Validating there is a waste on this path twice
    /// over: the bytes were validated on the way into the source column, and a copy cannot make
    /// valid bytes invalid. Reading a ClickBench partition spent eight percent of its cycles on
    /// that second validation.
    ///
    /// A position past the end of the source appends the empty string, which is what the copy loop
    /// wants for a row that resolved to nowhere.
    pub fn push_from(&mut self, source: &Self, index: usize) -> usize {
        self.push_bytes(source.bytes(index).unwrap_or(b""))
    }

    /// Appends bytes that are not required to be text, and returns their index.
    ///
    /// What a `BLOB` is stored through. The column is the same column either way, because a string
    /// here is already a length and some bytes and text is the reading rather than the storage, so
    /// a blob costs nothing extra and shares every kernel that works on views. What it does not
    /// share is [`Self::get`], which answers `None` for bytes that are not a string, so a caller
    /// holding blobs reads them with [`Self::bytes`].
    pub fn push_bytes(&mut self, bytes: &[u8]) -> usize {
        let offset = self.arena.len() as u64;
        if bytes.len() > INLINE_LIMIT {
            self.arena.extend_from_slice(bytes);
        }
        self.views.push(StringView::over(bytes, offset));
        self.views.len() - 1
    }

    /// Records a string that is already in the arena, and returns its index.
    ///
    /// The half of the seam that does the work. [`Self::over`] puts the page in, this says where in
    /// it a string is, and between them a column of long strings is built without the payload being
    /// touched at all.
    ///
    /// A string short enough to sit inside a view is copied into the view, which is at most twelve
    /// bytes and is what makes it readable without going near the arena at all. Everything longer
    /// keeps its bytes where they are and the view records the offset.
    ///
    /// # Errors
    ///
    /// If the range is not inside the arena, or if the bytes are not valid UTF-8. The validation is
    /// the one cost this seam does not remove, and it is here rather than skipped because
    /// [`Self::get`] hands back a `&str` and a column that cannot produce one for a string it claims
    /// to hold is a wrong answer rather than a slow one. A scan over a page where the format
    /// guarantees UTF-8 wants to validate the page once instead of once per string, which is a pass
    /// the layer three reader makes and is not something this type can do on its behalf.
    pub fn push_in_place(&mut self, offset: usize, len: usize) -> Result<usize> {
        let end = offset.checked_add(len).ok_or_else(|| {
            Error::internal(format!(
                "a string at {offset} of {len} bytes runs off the end of memory"
            ))
        })?;
        let bytes = self.arena.get(offset..end).ok_or_else(|| {
            Error::internal(format!(
                "a string at {offset} of {len} bytes is not inside a {} byte arena",
                self.arena.len()
            ))
        })?;
        let text = std::str::from_utf8(bytes)
            .map_err(|_| Error::internal(format!("the bytes at {offset} are not valid UTF-8")))?;
        let view = if len <= INLINE_LIMIT {
            StringView::inline(text)
        } else {
            StringView::indirect(text, offset as u64)
        };
        self.views.push(view);
        Ok(self.views.len() - 1)
    }

    /// The bytes the long strings live in.
    ///
    /// For a column over a page this is the page, including whatever of it no view points at. The
    /// offsets in the views are offsets into exactly this, which is what makes them meaningful to a
    /// reader that put the page here in the first place.
    #[must_use]
    pub fn arena(&self) -> &[u8] {
        &self.arena
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
        self.arena.get(view.offset()..view.offset() + view.len())
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

    /// Total bytes of payload held in the arena, which is what the memory accounting wants.
    ///
    /// For a column over a page it is the page and not the part of it any view points at, which is
    /// the right answer for accounting, because the page is what is resident.
    #[must_use]
    pub fn heap_bytes(&self) -> usize {
        self.arena.len()
    }

    /// Room for `bytes` of payload, taken in one allocation rather than as the strings arrive.
    ///
    /// A builder that knows the total byte count, which a scan reading a page and a gather copying a
    /// column both do, saves the doubling entirely. Nothing is wrong without it, which is why it is
    /// a hint and not a constructor argument.
    pub fn reserve_bytes(&mut self, bytes: usize) {
        self.arena.reserve(bytes);
    }
}

/// Two columns are equal when they hold the same strings in the same order, whatever their arenas
/// look like.
///
/// See the note on [`StringColumn`]. Comparing the views is not enough on its own either, because
/// two views of the same long string at different offsets in different arenas are different views,
/// so the comparison is length, then view by view with the payload read for the ones that are not
/// inline. The prefix inside the view is what makes that cheap: a pair that differs in the first
/// four bytes or in the length is settled without either arena being touched.
impl PartialEq for StringColumn {
    fn eq(&self, other: &Self) -> bool {
        self.views.len() == other.views.len()
            && (0..self.views.len()).all(|index| {
                let mine = self.views[index];
                let theirs = other.views[index];
                if mine.definitely_differs(&theirs) {
                    return false;
                }
                if mine.is_inline() {
                    return mine == theirs;
                }
                self.bytes(index) == other.bytes(index)
            })
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
    use crate::buffer::Buffer;

    /// The seam, used the way layer three will use it. The page arrives whole, each string is
    /// recorded where it already is, and the arena at the end is the page byte for byte, including
    /// the header this page has in front of the strings and the bytes between them that belong to
    /// nothing. A column that had copied would have an arena the size of the strings instead.
    #[test]
    fn a_column_over_a_page_records_the_strings_without_moving_them() {
        let page =
            b"HEADER..a string well past the inline limit!!a second one past the limit".to_vec();
        let mut column = StringColumn::over(Buffer::from_vec(page.clone()));
        assert_eq!(column.push_in_place(8, 37).expect("inside the page"), 0);
        assert_eq!(column.push_in_place(45, 27).expect("inside the page"), 1);
        assert_eq!(column.get(0), Some("a string well past the inline limit!!"));
        assert_eq!(column.get(1), Some("a second one past the limit"));
        assert_eq!(column.arena(), page.as_slice());
        assert_eq!(column.heap_bytes(), page.len());
        assert_eq!(column.len(), 2);
    }

    /// Copying between two columns, which is what a gather and a slice over a string column are.
    /// A column built over a page has an arena full of bytes no view points at, and the copy has to
    /// take the strings rather than the arena, so the destination holds the strings and nothing
    /// else. The last case is the row that resolved to nowhere, which is an empty string here and a
    /// null in the validity mask beside it.
    #[test]
    fn copying_from_another_column_takes_the_strings_and_not_the_page_they_were_in() {
        let page = b"HEADER..a string well past the inline limit!!short".to_vec();
        let mut source = StringColumn::over(Buffer::from_vec(page.clone()));
        source.push_in_place(8, 37).expect("inside the page");
        source.push_in_place(45, 5).expect("inside the page");

        let mut out = StringColumn::new();
        assert_eq!(out.push_from(&source, 1), 0);
        assert_eq!(out.push_from(&source, 0), 1);
        assert_eq!(out.push_from(&source, 9), 2, "a position that is not there");

        assert_eq!(out.get(0), Some("short"));
        assert_eq!(out.get(1), Some("a string well past the inline limit!!"));
        assert_eq!(out.get(2), Some(""));
        assert!(out.views()[0].is_inline(), "a short string stays in its view");
        assert!(!out.views()[1].is_inline());
        assert_eq!(out.views()[1].prefix(), *b"a st", "the prefix is the string's own");
        assert_eq!(
            out.arena(),
            b"a string well past the inline limit!!",
            "the arena is the long strings and not the page"
        );
    }

    /// Bytes that are not text, which is what a `BLOB` holds. Both sides of the inline limit,
    /// because a short one lives in its view and a long one lives in the arena and the byte that is
    /// not a character has to survive either way. Reading them back as text is `None` and reading
    /// them back as bytes is what went in.
    #[test]
    fn a_column_holds_bytes_that_are_not_a_string() {
        let long = b"\xff\xfe and a good deal more than twelve bytes of it";
        let mut column = StringColumn::new();
        assert_eq!(column.push_bytes(b"a\xffb"), 0);
        assert_eq!(column.push_bytes(long), 1);
        assert_eq!(column.push_bytes(b""), 2);

        assert_eq!(column.bytes(0), Some(b"a\xffb".as_slice()));
        assert_eq!(column.bytes(1), Some(long.as_slice()));
        assert_eq!(column.bytes(2), Some(b"".as_slice()));
        assert_eq!(column.get(0), None, "a stray 0xff is not a character");
        assert_eq!(column.get(1), None);
        assert!(column.views()[0].is_inline());
        assert!(!column.views()[1].is_inline());
        assert_eq!(column.arena(), long, "only the long one needed the arena");
    }

    /// A copy of a copy, because the second one reads its bytes out of an arena the first one wrote
    /// rather than out of a page, and an offset written in one and read in the other is the way
    /// this goes wrong.
    #[test]
    fn copying_from_a_column_that_was_itself_copied_reads_the_same_strings() {
        let mut first = StringColumn::new();
        for text in ["a string well past the inline limit", "short", "another long one past it"] {
            first.push(text);
        }
        let mut second = StringColumn::new();
        for index in (0..first.len()).rev() {
            second.push_from(&first, index);
        }
        let mut third = StringColumn::new();
        for index in 0..second.len() {
            third.push_from(&second, index);
        }
        assert_eq!(
            third.iter().collect::<Vec<_>>(),
            ["another long one past it", "short", "a string well past the inline limit"]
        );
    }

    /// A string short enough to live inside its view is copied into the view, which is twelve bytes
    /// and is what lets it be read without the arena. The page is still the arena and is still
    /// untouched, so a page of short strings costs the views and nothing else.
    #[test]
    fn a_short_string_in_a_page_is_copied_into_its_view() {
        let mut column = StringColumn::over(Buffer::from_vec(b"one.two".to_vec()));
        column.push_in_place(0, 3).expect("inside the page");
        column.push_in_place(4, 3).expect("inside the page");
        assert!(column.views()[0].is_inline());
        assert_eq!(column.get(0), Some("one"));
        assert_eq!(column.get(1), Some("two"));
        assert_eq!(column.arena(), b"one.two");
    }

    /// The two ways a caller can be wrong about a page, both of them answered before anything is
    /// recorded rather than at the point somebody reads the string back and finds nothing there.
    #[test]
    fn a_range_outside_the_page_or_bytes_that_are_not_text_are_refused() {
        let mut column = StringColumn::over(Buffer::from_vec(vec![0xff, 0xfe, 0xfd]));
        assert!(column.push_in_place(2, 4).is_err());
        assert!(column.push_in_place(usize::MAX, 1).is_err());
        assert!(column.push_in_place(0, 3).is_err());
        assert_eq!(column.len(), 0);
    }

    /// What the seam does to equality. The same two strings, one column built by copying them in
    /// and one built over a page that has them in the other order with a gap in the middle, and the
    /// two arenas have nothing in common. Equality is the strings, so the columns are equal.
    #[test]
    fn the_same_strings_over_different_arenas_are_the_same_column() {
        let copied: StringColumn =
            ["the first string past the limit", "the second string past the limit"]
                .into_iter()
                .collect();
        let page =
            b"gap!the second string past the limit....the first string past the limit".to_vec();
        let mut over = StringColumn::over(Buffer::from_vec(page));
        over.push_in_place(40, 31).expect("inside the page");
        over.push_in_place(4, 32).expect("inside the page");
        assert_ne!(copied.arena(), over.arena());
        assert_eq!(copied, over);

        let mut different: StringColumn = copied.clone();
        different.push("a third one past the inline limit");
        assert_ne!(copied, different);
    }

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

    /// A string of any size goes in whole, with the short ones on either side of it still reading
    /// back. The old layout had a size at which a string stopped fitting a block and got one of its
    /// own, and one arena has no such size, so the case worth keeping is the one that used to be
    /// special rather than the branch that used to handle it.
    #[test]
    fn a_string_far_larger_than_any_block_would_have_been_goes_in_whole() {
        let long = "x".repeat(40 * 1024);
        let mut column = StringColumn::new();
        column.push("short");
        column.push(&long);
        column.push("also short");
        assert_eq!(column.get(1), Some(long.as_str()));
        assert_eq!(column.get(2), Some("also short"));
        assert_eq!(column.heap_bytes(), long.len());
    }

    /// The property the whole arena rests on. Two thousand strings is tens of reallocations, and
    /// every one of them moves the bytes to a new address while the offsets recorded in the views
    /// before it stay exactly as they were. A view holding a pointer would be reading freed memory
    /// by the end of this test.
    #[test]
    fn the_arena_moving_underneath_does_not_move_what_the_views_point_at() {
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
    fn reserving_bytes_changes_nothing_but_where_the_allocation_happens() {
        let mut column = StringColumn::with_capacity(3);
        column.reserve_bytes(128);
        for text in ["a string past the limit", "another one past it", "short"] {
            column.push(text);
        }
        assert_eq!(column.get(0), Some("a string past the limit"));
        assert_eq!(column.get(1), Some("another one past it"));
        assert_eq!(column.get(2), Some("short"));
        assert_eq!(column.heap_bytes(), 42);
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
