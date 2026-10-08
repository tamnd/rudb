//! The part of a buffer a structure was read from, kept rather than copied.
//!
//! A forward link, an adjacency or a key map is read out of a section's payload, and most of the
//! payload is the packed rows at the end of it. Copying those rows into a buffer of their own wrote
//! every one of them to fresh memory a second time, and on TPC-H q21 loading the sections was about
//! half the page faults of a query whose system time was as large as its user time. Taking the
//! payload instead keeps the rows where the read put them, at the cost of the header in front of
//! them.
//!
//! The payload is [`Held`], so it can be bytes the reader never copied at all. A mapped file hands
//! out the page cache's own bytes for a section, and then the rows are read where the file keeps
//! them.

use std::ops::Deref;
use std::sync::Arc;

/// The bytes a structure was read from, shared by everything read out of them.
///
/// A `Vec<u8>` is one, and so is a range of a mapped file. Whatever it is has to hand back the same
/// bytes every time it is asked, which a buffer that is never written to does.
pub type Held = Arc<dyn AsRef<[u8]> + Send + Sync>;

/// Bytes `at..end` of `held`.
///
/// Asking for the bytes goes through `held`, which is a call that cannot be inlined, so a loop over
/// them takes the slice once before it starts rather than once a row.
#[derive(Clone)]
pub(crate) struct Tail {
    held: Held,
    at: usize,
    end: usize,
}

impl Tail {
    /// The bytes of `held` from `at` on, or `None` when `at` is past its end.
    pub(crate) fn of(held: Held, at: usize) -> Option<Self> {
        let end = (*held).as_ref().len();
        Self::within(held, at, end)
    }

    /// The bytes of `held` from `at` to `end`, or `None` when they are not inside it.
    pub(crate) fn within(held: Held, at: usize, end: usize) -> Option<Self> {
        (at <= end && end <= (*held).as_ref().len()).then_some(Self { held, at, end })
    }
}

impl From<Vec<u8>> for Tail {
    fn from(held: Vec<u8>) -> Self {
        let end = held.len();
        Self { held: Arc::new(held), at: 0, end }
    }
}

impl Default for Tail {
    fn default() -> Self {
        Self::from(Vec::new())
    }
}

impl Deref for Tail {
    type Target = [u8];

    fn deref(&self) -> &[u8] {
        &(*self.held).as_ref()[self.at..self.end]
    }
}

impl std::fmt::Debug for Tail {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Tail").field("len", &self.len()).finish()
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use super::{Held, Tail};

    fn held(bytes: Vec<u8>) -> Held {
        Arc::new(bytes)
    }

    #[test]
    fn a_tail_is_the_bytes_past_its_start_and_a_clone_shares_them() {
        let tail = Tail::of(held(vec![1, 2, 3, 4, 5]), 2).expect("inside the buffer");
        assert_eq!(&*tail, &[3, 4, 5]);
        let copy = tail.clone();
        assert_eq!(&*copy, &[3, 4, 5]);
        assert_eq!(Arc::strong_count(&copy.held), 2);
        assert!(Tail::of(held(vec![1, 2]), 3).is_none());
        assert!(Tail::of(held(vec![1, 2]), 2).is_some_and(|tail| tail.is_empty()));
    }

    #[test]
    fn a_range_is_the_bytes_between_its_ends() {
        let range = Tail::within(held(vec![1, 2, 3, 4, 5]), 1, 3).expect("inside the buffer");
        assert_eq!(&*range, &[2, 3]);
        assert!(Tail::within(held(vec![1, 2, 3]), 2, 4).is_none());
        assert!(Tail::within(held(vec![1, 2, 3]), 3, 2).is_none());
    }
}
