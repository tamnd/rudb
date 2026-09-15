//! The run of values behind a flat vector, and the seam the buffer manager arrives through.
//!
//! `spec/engine/03-data-plane.md` section 3.8. Today every vector owns its payload and every scan
//! allocates, which is the right place to start and is not where this ends. At layer three the scan
//! reads a page out of the buffer manager and the vector wants to point into that page rather than
//! copy out of it, and the copy it avoids is the largest single copy in the system, because it is
//! every byte of every column every query reads.
//!
//! The type that supports both is one enum holding either an owned run or a borrowed one, and the
//! part that has to be decided correctly the first time is how the borrow is expressed, because that
//! shows up in every signature that mentions a vector.
//!
//! # Why the pin is a handle and not a lifetime
//!
//! The obvious way to express a borrow in Rust is a lifetime parameter, and it is the wrong one
//! here. A lifetime on [`Buffer`] is a lifetime on [`Data`](crate::Data), which is a lifetime on
//! [`Vector`](crate::Vector), which is a lifetime on [`Chunk`](crate::Chunk), which is a lifetime on
//! every operator's state, on every trait object in the pipeline, and on every queue a chunk is put
//! into for another thread to pick up. The scheduler is exactly that last thing, so the borrow would
//! have to outlive a hand off between threads that the compiler has no way to see the end of. The
//! two ways out of that are unsafe code and a copy at the boundary, and the copy at the boundary is
//! the thing the borrow existed to avoid.
//!
//! So the pin is a [`Pin`], a reference counted handle the buffer holds, and the page stays alive
//! because the handle is alive rather than because a region ends. It costs one atomic increment per
//! vector construction, which is not measurable next to reading the page it is protecting, and the
//! ownership story stays uniform: a chunk is `Send`, always, whatever its columns are pointing at.
//!
//! # Why it landed with one variant
//!
//! There was no buffer manager, so there was nothing to borrow from, and writing the borrowed
//! variant then would have been writing an interface against an imaginary caller. What landed was
//! the enum, with only the owned variant in it, so that adding a variant later is a change inside
//! this crate rather than a change to every signature in the workspace. The reader side of that
//! migration was already done by the [`Deref`] below: everything outside this crate reads a slice,
//! and a slice is what every variant hands back. The section after this one is that bet being
//! collected, and it cost two functions.
//!
//! The writer side is [`Buffer::to_mut`], which is the one function that has to grow a case. A write
//! through a borrowed buffer has to copy the page into an owned run first, which is what `Cow` does
//! and for the same reason, and having the call site named now means that day is a change to one
//! function rather than a search for every `push`.
//!
//! # The second variant arrived early, and from the other direction
//!
//! [`Store::Shared`] is here before the buffer manager is, because the Parquet reader needed the
//! same thing for a different reason. A string column is built over the page it was decoded from
//! rather than copying out of it, so the page becomes the column's arena and goes downstream with
//! it, and the reader never gets the allocation back. On `hits` that is ten and a half megabytes a
//! row group, freed and taken again for every row group, and glibc hands a block that size back to
//! the kernel when it is freed, so the next one faults in every page of it. Measured on the URL
//! column of `hits-1m-snappy.parquet`, running with `MALLOC_MMAP_THRESHOLD_` and
//! `MALLOC_TRIM_THRESHOLD_` both raised so that nothing is ever handed back took the query from
//! 113.08 milliseconds to 104.08, and the decode stage moved as well as the decompress one, which
//! is what says it is page faults rather than anything about the codec.
//!
//! So the arena is held by an [`Arc`] and the reader keeps a handle to it. When every column built
//! over that page has been dropped the reader is the only holder left, takes the run back out and
//! decompresses the next page into it. That is a page pool with two entries and no eviction policy,
//! which is not the buffer manager, but it is the same shape and it is the first caller that will
//! want one.
//!
//! It is [`Arc<Vec<T>>`] rather than [`Pin`] because this caller knows exactly what its page is and
//! can say so in the type. The [`Pin`] variant is still coming and is still opaque, because the
//! buffer manager's page is a frame in a pool that a vector has no business knowing the shape of.
//! Two variants for two situations is the honest answer here: one of them can name its page and the
//! other cannot.

use std::any::Any;
use std::ops::Deref;
use std::sync::Arc;

/// What keeps a page alive for as long as a buffer points into it.
///
/// Opaque on purpose. The vector does not know what a page is and has no business looking inside
/// one, it only has to hold the thing that stops the page being evicted, and the buffer manager at
/// layer three decides what that thing is. The bounds are the load bearing part and they are here
/// now: `Send` and `Sync`, because a chunk carrying one of these crosses a thread boundary every
/// time the scheduler moves a pipeline, and `'static`, because a handle with a lifetime on it would
/// have put the lifetime back on [`Vector`](crate::Vector) by another route.
pub type Pin = Arc<dyn Any + Send + Sync>;

/// A run of values of one physical type.
///
/// Derefs to a slice, which is how every reader in the workspace gets at it, so a reader does not
/// know or care which variant it is holding. Writers go through [`Self::push`] and
/// [`Self::to_mut`].
#[derive(Debug, Clone)]
pub struct Buffer<T> {
    store: Store<T>,
}

/// Where the values actually are.
///
/// The third is a run inside a page, carrying the [`Pin`] that keeps the page where it is, and it
/// arrives with the buffer manager because that is the only thing that can hand one out. It is an
/// enum rather than a newtype around `Vec<T>` because the shape is what the rest of the workspace
/// is compiled against, and a newtype that turns into an enum later is the flag day this exists to
/// avoid.
#[derive(Debug, Clone)]
enum Store<T> {
    /// The vector owns the values.
    Owned(Vec<T>),
    /// The values are a whole page somebody else is holding a handle to as well.
    ///
    /// Read only, which is not enforced and does not need to be: the only way to write is
    /// [`Buffer::to_mut`] and that copies out first, so a shared page is never written through even
    /// by a caller that has forgotten what it is holding.
    Shared(Arc<Vec<T>>),
}

impl<T> Buffer<T> {
    /// An empty buffer.
    #[must_use]
    pub fn new() -> Self {
        Self { store: Store::Owned(Vec::new()) }
    }

    /// An empty buffer with room for `capacity` values.
    #[must_use]
    pub fn with_capacity(capacity: usize) -> Self {
        Self { store: Store::Owned(Vec::with_capacity(capacity)) }
    }

    /// A buffer owning `values`.
    #[must_use]
    pub fn from_vec(values: Vec<T>) -> Self {
        Self { store: Store::Owned(values) }
    }

    /// A buffer over a whole page that somebody else is holding a handle to as well.
    ///
    /// The caller that wants this is one that decoded the page and is going to want the allocation
    /// back when the last reader of it is gone, which it gets by keeping its own handle and waiting
    /// for [`Arc::get_mut`] to start answering. Nothing here enforces that, and a caller that drops
    /// its handle has simply built an owned buffer with an extra indirection.
    #[must_use]
    pub fn from_arc(page: Arc<Vec<T>>) -> Self {
        Self { store: Store::Shared(page) }
    }

    /// Whether this buffer is a page it shares rather than a run it owns.
    ///
    /// For a caller deciding whether a write is about to cost a copy of the page, and for the tests
    /// that assert the reader did not quietly stop sharing.
    #[must_use]
    pub fn is_shared(&self) -> bool {
        matches!(self.store, Store::Shared(_))
    }

    /// How many bytes of memory this buffer is holding.
    ///
    /// Capacity rather than length, because capacity is what was taken from the allocator.
    ///
    /// A shared page is charged to the buffers over it in equal parts, which is an approximation
    /// and is worth being plain about. The exact answer needs to know who else is holding the page
    /// and what they are charging, and no buffer can see that. Splitting it means the live buffers
    /// over one page add up to slightly less than the page, never to several times it, and that is
    /// the direction to be wrong in: a scan that emits fifty chunks over one ten megabyte page
    /// would otherwise report half a gigabyte and trip a memory limit that nothing came close to.
    #[must_use]
    pub fn footprint(&self) -> usize {
        match &self.store {
            Store::Owned(values) => values.capacity() * size_of::<T>(),
            Store::Shared(page) => page.capacity() * size_of::<T>() / Arc::strong_count(page),
        }
    }

    /// The values.
    #[must_use]
    #[inline]
    pub fn as_slice(&self) -> &[T] {
        match &self.store {
            Store::Owned(values) => values,
            Store::Shared(page) => page,
        }
    }
}

impl<T: Clone> Buffer<T> {
    /// The values as an owned run, copying only if they were not owned already.
    ///
    /// One of the two places that can copy a page, and it is spelled as a method rather than a
    /// field access so that it is greppable. The copy is skipped when the caller turns out to hold
    /// the last handle to the page, which is the common case for a buffer that was shared only so
    /// that its producer could get the allocation back.
    #[must_use]
    pub fn into_vec(self) -> Vec<T> {
        match self.store {
            Store::Owned(values) => values,
            Store::Shared(page) => Arc::try_unwrap(page).unwrap_or_else(|page| page.to_vec()),
        }
    }

    /// The values, writable, copying them out of the page first if they are not owned.
    ///
    /// The copy on write point, and the only one. Everything that mutates a buffer goes through
    /// here, so a variant that is not owned needs a case in this function and in nothing else.
    #[inline]
    pub fn to_mut(&mut self) -> &mut Vec<T> {
        if let Store::Shared(page) = &self.store {
            self.store = Store::Owned(page.to_vec());
        }
        match &mut self.store {
            Store::Owned(values) => values,
            // A page cannot be here: the line above just replaced it.
            Store::Shared(_) => unreachable!("a shared page was copied out one statement ago"),
        }
    }

    /// Appends one value.
    #[inline]
    pub fn push(&mut self, value: T) {
        self.to_mut().push(value);
    }

    /// Room for `additional` more values, taken in one allocation.
    pub fn reserve(&mut self, additional: usize) {
        self.to_mut().reserve(additional);
    }

    /// Appends a run of values.
    pub fn extend_from_slice(&mut self, values: &[T]) {
        self.to_mut().extend_from_slice(values);
    }
}

/// Equality is the values and not where they live.
///
/// Derived equality would call an owned run different from a page holding the same values, which
/// would make every test in the workspace that compares two vectors assert on how the vector was
/// built. [`crate::StringColumn`] settles the same question the same way and for the same reason.
impl<T: PartialEq> PartialEq for Buffer<T> {
    fn eq(&self, other: &Self) -> bool {
        self.as_slice() == other.as_slice()
    }
}

impl<T: Eq> Eq for Buffer<T> {}

impl<T> Default for Buffer<T> {
    fn default() -> Self {
        Self::new()
    }
}

impl<T> Deref for Buffer<T> {
    type Target = [T];

    #[inline]
    fn deref(&self) -> &[T] {
        self.as_slice()
    }
}

impl<T> From<Vec<T>> for Buffer<T> {
    fn from(values: Vec<T>) -> Self {
        Self::from_vec(values)
    }
}

impl<T> FromIterator<T> for Buffer<T> {
    fn from_iter<I: IntoIterator<Item = T>>(iter: I) -> Self {
        Self::from_vec(iter.into_iter().collect())
    }
}

impl<T: Clone> Extend<T> for Buffer<T> {
    fn extend<I: IntoIterator<Item = T>>(&mut self, iter: I) {
        self.to_mut().extend(iter);
    }
}

impl<T: Clone> IntoIterator for Buffer<T> {
    type Item = T;
    type IntoIter = std::vec::IntoIter<T>;

    fn into_iter(self) -> Self::IntoIter {
        self.into_vec().into_iter()
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use super::{Buffer, Pin};

    #[test]
    fn a_buffer_reads_back_as_a_slice() {
        let buffer: Buffer<i32> = vec![1, 2, 3].into();
        assert_eq!(buffer.as_slice(), &[1, 2, 3]);
        assert_eq!(buffer.len(), 3);
        assert_eq!(buffer[1], 2);
        assert_eq!(buffer.iter().sum::<i32>(), 6);
        assert_eq!(buffer.clone().into_vec(), vec![1, 2, 3]);
    }

    #[test]
    fn writing_goes_through_one_function() {
        let mut buffer = Buffer::with_capacity(4);
        buffer.push(1u8);
        buffer.extend_from_slice(&[2, 3]);
        buffer.extend([4u8]);
        buffer.to_mut().sort_unstable_by(|a, b| b.cmp(a));
        assert_eq!(buffer.as_slice(), &[4, 3, 2, 1]);
    }

    #[test]
    fn an_empty_buffer_is_the_default_and_collects_like_a_vector() {
        assert!(Buffer::<u64>::default().is_empty());
        assert!(Buffer::<u64>::new().is_empty());
        let collected: Buffer<u64> = (0..4).collect();
        assert_eq!(collected.as_slice(), &[0, 1, 2, 3]);
        assert_eq!(collected.into_iter().count(), 4);
    }

    #[test]
    fn a_shared_page_reads_like_an_owned_run_and_compares_equal_to_one() {
        let page = Arc::new(vec![1u8, 2, 3]);
        let buffer = Buffer::from_arc(Arc::clone(&page));
        assert!(buffer.is_shared());
        assert_eq!(buffer.as_slice(), &[1, 2, 3]);
        assert_eq!(buffer[2], 3);
        assert_eq!(buffer, Buffer::from_vec(vec![1u8, 2, 3]));
        assert_eq!(Buffer::from_vec(vec![1u8, 2, 3]), buffer);
        assert_ne!(buffer, Buffer::from_vec(vec![1u8, 2]));
    }

    /// The point of the variant. The producer keeps a handle, the reader drops its buffer, and the
    /// producer gets the allocation back rather than the allocator getting it.
    #[test]
    fn the_producer_gets_the_page_back_once_the_last_buffer_over_it_is_gone() {
        let mut page = Arc::new(vec![0u8; 64]);
        let address = page.as_ptr();
        let first = Buffer::from_arc(Arc::clone(&page));
        let second = Buffer::from_arc(Arc::clone(&page));
        assert!(Arc::get_mut(&mut page).is_none());
        drop(first);
        assert!(Arc::get_mut(&mut page).is_none());
        drop(second);
        let run = Arc::get_mut(&mut page).expect("the last handle");
        assert_eq!(run.as_ptr(), address, "the page was reallocated rather than reused");
    }

    /// Writing through a shared page copies it out, and the page the producer is holding is left
    /// exactly as it was. The one case where getting this wrong would corrupt another reader.
    #[test]
    fn writing_through_a_shared_page_copies_it_and_leaves_the_page_alone() {
        let page = Arc::new(vec![1u8, 2, 3]);
        let mut buffer = Buffer::from_arc(Arc::clone(&page));
        buffer.push(4);
        assert!(!buffer.is_shared());
        assert_eq!(buffer.as_slice(), &[1, 2, 3, 4]);
        assert_eq!(page.as_slice(), &[1, 2, 3]);
        assert_eq!(Buffer::from_arc(Arc::clone(&page)).into_vec(), vec![1, 2, 3]);
    }

    /// Taking the run out of the last handle to a page does not copy it, which is what makes the
    /// shared variant free for a caller that ends up being the only reader after all.
    #[test]
    fn the_last_buffer_over_a_page_takes_the_run_without_copying_it() {
        let page = Arc::new(vec![5u8; 32]);
        let address = page.as_ptr();
        let run = Buffer::from_arc(page).into_vec();
        assert_eq!(run.as_ptr(), address);
    }

    /// The accounting rule from [`Buffer::footprint`], which is that the buffers over a page add up
    /// to at most the page rather than to a multiple of it.
    #[test]
    fn a_shared_page_is_charged_once_across_the_buffers_over_it() {
        let page = Arc::new(vec![0u64; 100]);
        let over: Vec<_> = (0..4).map(|_| Buffer::from_arc(Arc::clone(&page))).collect();
        let charged: usize = over.iter().map(Buffer::footprint).sum();
        assert!(
            charged <= page.capacity() * 8,
            "{charged} charged for a {} byte page",
            page.len() * 8
        );
        assert!(charged > 0);
        assert_eq!(Buffer::from_vec(vec![0u64; 100]).footprint(), 800);
    }

    /// The property the whole section 3.8 decision is about. A buffer of any payload can be sent to
    /// another thread without a lifetime being involved, and so can a pin, which is what makes a
    /// chunk `Send` once the borrowed variant exists. Asserted rather than assumed, because a pin
    /// that was an `Rc` would compile everywhere else and fail here.
    #[test]
    fn a_buffer_and_a_pin_both_cross_a_thread_boundary() {
        const fn assert_send<T: Send>() {}
        assert_send::<Buffer<i64>>();
        assert_send::<Pin>();
        let pin: Pin = Arc::new(vec![0u8; 8]);
        let buffer: Buffer<i64> = vec![7; 2].into();
        let handle = std::thread::spawn(move || (buffer.len(), Arc::strong_count(&pin)));
        assert_eq!(handle.join().expect("the thread"), (2, 1));
    }
}
