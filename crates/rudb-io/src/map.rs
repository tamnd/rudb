//! A file mapped read only, so a reader can take bytes where the page cache holds them.
//!
//! A positional read copies. It needs a buffer to copy into, and a buffer the size of a column page
//! is fresh memory the kernel zeroes a page at a time before the read fills it, so one read of a
//! cached page costs a fault and a zeroing per four kilobytes and then the copy itself. On TPC-H at
//! one thread that was a fifth to nearly half of the CPU time, in the kernel, for bytes that were
//! already in memory. A mapping hands out the page cache's own bytes, so the only cost left is the
//! fault that maps them, and the kernel maps sixteen pages per fault around the one touched.
//!
//! The mapping covers the file as long as it was when it was opened. The caller promises that the
//! bytes it asks for are never rewritten in place and that the file is never cut shorter than the
//! mapping while it is held. The native format keeps both: a committed page is never written again,
//! a rewrite goes to a new file that is renamed over the old one, and nothing truncates. The header
//! slots are written in place, so those are read with a positional read and not through here.
//!
//! Where a mapping cannot be made, [`Mapped::open`] says so with `None` and the caller reads the way
//! it did before. That is every platform that is not unix, and an empty file.

use std::fs::File;

/// A read only mapping of a file's first `len` bytes.
#[derive(Debug)]
pub struct Mapped {
    at: *const u8,
    len: usize,
}

// SAFETY: the mapping is read only and does not belong to a thread, so it can be unmapped on any.
#[allow(unsafe_code, reason = "a raw pointer to shared read only memory is Send and Sync")]
unsafe impl Send for Mapped {}
// SAFETY: the mapping is read only and never moves, so a shared reference to it from any thread
// reads the same bytes, and unmapping it takes the last owner in `Drop`.
#[allow(unsafe_code, reason = "a raw pointer to shared read only memory is Send and Sync")]
unsafe impl Sync for Mapped {}

impl Mapped {
    /// Maps the first `len` bytes of `file`, or `None` where a mapping cannot be made.
    #[must_use]
    pub fn open(file: &File, len: u64) -> Option<Self> {
        let len = usize::try_from(len).ok().filter(|&len| len > 0)?;
        sys::map(file, len).map(|at| Self { at, len })
    }

    /// The `len` bytes at `offset`, or `None` when they run past the end of the mapping.
    #[must_use]
    pub fn get(&self, offset: u64, len: usize) -> Option<&[u8]> {
        let start = usize::try_from(offset).ok()?;
        let end = start.checked_add(len)?;
        (end <= self.len).then(|| {
            // SAFETY: `start..end` lies inside the mapping, which is readable and lives as long as
            // `self`, and nothing writes to those bytes while it is held. See the module comment.
            #[allow(unsafe_code, reason = "the slice is inside a live read only mapping")]
            unsafe {
                std::slice::from_raw_parts(self.at.add(start), len)
            }
        })
    }

    /// Lets the process stop holding the pages under `offset..offset + len`, once their bytes have
    /// been read and copied out.
    ///
    /// A scan reads each part of a column once and decodes it into memory of its own, and without
    /// this every page it read stayed mapped into the process for as long as the file did. That is
    /// the whole of every column a query touched, counted against it as resident memory: 17 MB of
    /// the 61 MB ClickBench 28 held at its peak, for bytes nothing would read again. The page cache
    /// still has them, so a read that does come back faults them in again and sees the same bytes.
    ///
    /// Pages are dropped whole, so a page shared with the part next to it goes as well, and the
    /// next part pays one fault to bring it back. That is always safe, since the mapping is read
    /// only and the file under it never changes.
    pub fn release(&self, offset: u64, len: usize) {
        let Ok(start) = usize::try_from(offset) else { return };
        let end = start.saturating_add(len).min(self.len);
        if start >= end {
            return;
        }
        sys::release(self.at, start, end);
    }

    /// How many bytes it maps.
    #[must_use]
    pub fn len(&self) -> usize {
        self.len
    }

    /// Whether it maps nothing, which it never does, since [`Mapped::open`] refuses an empty file.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.len == 0
    }
}

impl Drop for Mapped {
    fn drop(&mut self) {
        sys::unmap(self.at, self.len);
    }
}

#[cfg(unix)]
#[allow(unsafe_code, reason = "mmap and munmap have no wrapper in std")]
mod sys {
    use std::ffi::{c_int, c_void};
    use std::fs::File;
    use std::os::fd::AsRawFd;

    unsafe extern "C" {
        fn mmap(
            addr: *mut c_void,
            len: usize,
            prot: c_int,
            flags: c_int,
            fd: c_int,
            offset: i64,
        ) -> *mut c_void;
        fn munmap(addr: *mut c_void, len: usize) -> c_int;
        #[cfg(target_os = "linux")]
        fn madvise(addr: *mut c_void, len: usize, advice: c_int) -> c_int;
    }

    /// Linux's `MADV_DONTNEED`, which for a shared file mapping drops the process's hold on the
    /// pages and leaves them in the page cache.
    #[cfg(target_os = "linux")]
    const MADV_DONTNEED: c_int = 4;

    /// The alignment a release starts on, a multiple of every page size Linux runs with here.
    #[cfg(target_os = "linux")]
    const RELEASE_ALIGN: usize = 64 * 1024;

    const PROT_READ: c_int = 1;
    const MAP_SHARED: c_int = 1;

    pub(super) fn map(file: &File, len: usize) -> Option<*const u8> {
        // SAFETY: a fresh mapping at an address the kernel picks, of a descriptor that is open for
        // the length of the call. It aliases nothing Rust owns.
        let at =
            unsafe { mmap(std::ptr::null_mut(), len, PROT_READ, MAP_SHARED, file.as_raw_fd(), 0) };
        // MAP_FAILED is all ones.
        (at as usize != usize::MAX && !at.is_null()).then_some(at.cast_const().cast())
    }

    #[cfg(target_os = "linux")]
    pub(super) fn release(at: *const u8, start: usize, end: usize) {
        // The mapping itself starts on a page, so rounding the address down stays inside it as
        // long as it does not go below the start.
        let base = at as usize;
        let from = ((base + start) & !(RELEASE_ALIGN - 1)).max(base);
        // SAFETY: `from..base + end` lies inside the mapping, which is read only and shared, so
        // dropping its pages changes nothing a later read sees: the next touch maps them again out
        // of the page cache.
        unsafe {
            madvise(from as *mut c_void, base + end - from, MADV_DONTNEED);
        }
    }

    #[cfg(not(target_os = "linux"))]
    pub(super) fn release(_: *const u8, _: usize, _: usize) {}

    pub(super) fn unmap(at: *const u8, len: usize) {
        // SAFETY: `at` and `len` are what `map` returned and was asked for, and the last reference
        // into the mapping ended with the `Mapped` that is being dropped.
        unsafe {
            munmap(at.cast_mut().cast(), len);
        }
    }
}

#[cfg(not(unix))]
mod sys {
    use std::fs::File;

    pub(super) fn map(_: &File, _: usize) -> Option<*const u8> {
        None
    }

    pub(super) fn unmap(_: *const u8, _: usize) {}

    pub(super) fn release(_: *const u8, _: usize, _: usize) {}
}

#[cfg(all(test, unix))]
mod tests {
    use std::io::Write;

    use super::Mapped;

    #[test]
    fn a_mapping_reads_the_bytes_a_read_would() {
        let path = std::env::temp_dir().join(format!("rudb-map-{}", std::process::id()));
        let bytes: Vec<u8> = (0..20_000u32).map(|i| (i % 251) as u8).collect();
        std::fs::File::create(&path).unwrap().write_all(&bytes).unwrap();
        let file = std::fs::File::open(&path).unwrap();
        let mapped = Mapped::open(&file, bytes.len() as u64).unwrap();
        assert_eq!(mapped.len(), bytes.len());
        assert_eq!(mapped.get(0, bytes.len()).unwrap(), &bytes[..]);
        assert_eq!(mapped.get(4_097, 9_000).unwrap(), &bytes[4_097..13_097]);
        assert_eq!(mapped.get(19_999, 1).unwrap(), &bytes[19_999..]);
        assert!(mapped.get(19_999, 2).is_none());
        assert!(mapped.get(u64::MAX, 1).is_none());
        std::fs::remove_file(&path).unwrap();
    }

    #[test]
    fn an_empty_file_is_not_mapped() {
        let path = std::env::temp_dir().join(format!("rudb-map-empty-{}", std::process::id()));
        std::fs::File::create(&path).unwrap();
        let file = std::fs::File::open(&path).unwrap();
        assert!(Mapped::open(&file, 0).is_none());
        std::fs::remove_file(&path).unwrap();
    }
}
