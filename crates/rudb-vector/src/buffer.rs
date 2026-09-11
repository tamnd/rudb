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
//! # Why it lands now with one variant
//!
//! There is no buffer manager, so there is nothing to borrow from, and writing the borrowed variant
//! now would be writing an interface against an imaginary caller. What lands now is the enum, with
//! only the owned variant in it, so that adding the second variant at layer three is a change inside
//! this crate rather than a change to every signature in the workspace. The reader side of that
//! migration is already done by the [`Deref`] below: everything outside this crate reads a slice, and
//! a slice is what both variants will hand back.
//!
//! The writer side is [`Buffer::to_mut`], which is the one function that has to grow a case. A write
//! through a borrowed buffer has to copy the page into an owned run first, which is what `Cow` does
//! and for the same reason, and having the call site named now means that day is a change to one
//! function rather than a search for every `push`.

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
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Buffer<T> {
    store: Store<T>,
}

/// Where the values actually are.
///
/// One variant today. The second is a run inside a page, carrying the [`Pin`] that keeps the page
/// where it is, and it arrives with the buffer manager because that is the only thing that can hand
/// one out. It is an enum with one arm rather than a newtype around `Vec<T>` because the shape is
/// what the rest of the workspace is being compiled against, and a newtype that turns into an enum
/// later is the flag day this exists to avoid.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Store<T> {
    /// The vector owns the values.
    Owned(Vec<T>),
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

    /// How many bytes of memory this buffer is holding.
    ///
    /// Capacity rather than length, because capacity is what was taken from the allocator. The day
    /// a buffer can be a run inside a pinned page this stops being the whole story, since the page
    /// is charged once by whoever pinned it and a hundred buffers over it are charged nothing, and
    /// this is the one function that has to know the difference.
    #[must_use]
    pub fn footprint(&self) -> usize {
        match &self.store {
            Store::Owned(values) => values.capacity() * size_of::<T>(),
        }
    }

    /// The values.
    #[must_use]
    #[inline]
    pub fn as_slice(&self) -> &[T] {
        match &self.store {
            Store::Owned(values) => values,
        }
    }

    /// The values as an owned run, copying only if they were not owned already.
    ///
    /// Free today because everything is owned. The day it is not, this is one of the two places that
    /// can copy a page, and it is spelled as a method rather than a field access so that it is
    /// greppable when that day comes.
    #[must_use]
    pub fn into_vec(self) -> Vec<T> {
        match self.store {
            Store::Owned(values) => values,
        }
    }

    /// The values, writable, copying them out of the page first if they are not owned.
    ///
    /// The copy on write point, and the only one. Everything that mutates a buffer goes through
    /// here, so the borrowed variant needs a case in this function and in nothing else.
    #[inline]
    pub fn to_mut(&mut self) -> &mut Vec<T> {
        match &mut self.store {
            Store::Owned(values) => values,
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
}

impl<T: Clone> Buffer<T> {
    /// Appends a run of values.
    pub fn extend_from_slice(&mut self, values: &[T]) {
        self.to_mut().extend_from_slice(values);
    }
}

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

impl<T> Extend<T> for Buffer<T> {
    fn extend<I: IntoIterator<Item = T>>(&mut self, iter: I) {
        self.to_mut().extend(iter);
    }
}

impl<T> IntoIterator for Buffer<T> {
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
