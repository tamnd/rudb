//! Files, and the interception shim the crash tests drive.
//!
//! Rank 1 in the layer rule. See `xtask/layers.toml` and `spec/18-package-layout.md`.
//!
//! Everything that touches a file in this project goes through [`Filesystem`] and [`File`]. Not
//! most things, everything. `spec/16-testing.md` section 16.5 is explicit about why the shim is
//! scheduled at M0 and not at M6, where the crash tests that use it live: retrofitting an
//! interception layer into a codebase that has been calling `File::write` directly for two years is
//! a much larger job than building against it from the start. So the shim goes in before there is
//! anything to intercept, and the rule that nothing bypasses it is cheap to keep now and expensive
//! to establish later.
//!
//! # What is here
//!
//! [`RealFilesystem`], which is `std::fs` and positional reads and writes.
//!
//! [`SimFilesystem`], which is memory, and which records every operation, can be told to fail at a
//! chosen point, and models the thing that actually happens on a crash: writes that were not
//! separated by an `fsync` can land in any combination.
//!
//! [`Request`] and [`Completion`], which are how a caller states every read it wants in one call
//! instead of one at a time. `spec/engine/05-scan.md` section 5.3 has the argument and
//! [`submit`] has the details.
//!
//! [`Pool`], which is the threads that serve those requests. They are not the execution threads,
//! which is the whole idea: a thread blocked on a read is not a core lost to execution, because the
//! thread that blocked was never an execution thread.
//!
//! # What is not here yet
//!
//! Direct I/O, io_uring and object storage. `spec/05-storage.md` sections on I/O say the layer ends
//! up with two backends chosen by measurement at startup, and choosing needs a buffer manager to
//! generate the depth and a workload to measure. What matters now is that the interface they will
//! implement exists and that nothing is written against `std::fs` directly in the meantime.
//!
//! # Why the methods take `&self`
//!
//! Positional I/O does not need exclusive access and the buffer manager is going to want many
//! readers at once. `read_at` and `write_at` are the whole interface for a reason: a seek plus a
//! read is two operations with shared state between them, and shared mutable state in the I/O layer
//! is how a database gets a bug that only appears at sixteen threads.

#![deny(unsafe_code)]

pub mod glob;
pub mod machine;
pub mod pool;
pub mod real;
pub mod sim;
pub mod submit;

#[cfg(test)]
mod scratch;

use std::fmt::Debug;
use std::path::{Path, PathBuf};

use rudb_common::Result;

pub use glob::expand;
pub use machine::{default_memory_limit, physical_memory};
pub use pool::{Config, Pool, Pooled, Stats};
pub use real::RealFilesystem;
pub use sim::{Completions, Crash, Op, SimFilesystem};
pub use submit::{Completion, Filler, Request, Response};

/// How a file is opened.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OpenMode {
    /// Must exist. Reads only, and a write is an error.
    Read,
    /// Must exist. Reads and writes.
    ReadWrite,
    /// Created if it does not exist, opened if it does.
    Create,
    /// Created, and an error if it already exists.
    ///
    /// The one that matters for a database file, because "create the database" and "open the
    /// database that is already there" are different intentions and collapsing them is how a
    /// process ends up writing a header over somebody's data.
    CreateNew,
}

impl OpenMode {
    /// Whether a write through a handle opened this way is allowed.
    #[must_use]
    pub fn writable(self) -> bool {
        !matches!(self, Self::Read)
    }
}

/// An open file, addressed by offset rather than by a cursor.
///
/// Implementors are shared across threads, which is why every method takes `&self`. A `File` here
/// is closer to a block device with a name than to `std::fs::File`.
pub trait File: Debug + Send + Sync {
    /// Reads into `buf` starting at `offset` and returns how many bytes were read.
    ///
    /// A short read at the end of the file is not an error, it is a short read. The caller knows
    /// how long the file is and what it expected.
    ///
    /// # Errors
    ///
    /// If the underlying read fails.
    fn read_at(&self, offset: u64, buf: &mut [u8]) -> Result<usize>;

    /// States every read the caller wants and hands back something to wait on or poll.
    ///
    /// This is the interface a scan is written against, per `spec/engine/05-scan.md` section 5.3.
    /// A row group scan knows all of its byte ranges before it reads any of them, so it says all of
    /// them at once, and a backend that hears all of them at once can issue them concurrently and
    /// can coalesce the adjacent ones. Neither is available to a caller that asks one range at a
    /// time, which is the whole reason the method exists.
    ///
    /// The default here is the loop over [`Self::read_at`], so every backend has a correct
    /// implementation from the moment it exists and a backend with a real queue underneath it
    /// overrides this rather than being the only thing that works. A failed read fails that one
    /// request and leaves the rest of the batch alone, because the caller may well be able to
    /// answer the query from what did arrive, and in any case it is the caller that knows.
    fn submit(&self, requests: Vec<Request>) -> Completion {
        let (completion, filler) = Completion::pending(requests.len());
        for (index, request) in requests.into_iter().enumerate() {
            let offset = request.offset();
            let mut buf = request.into_buffer();
            let outcome =
                self.read_at(offset, &mut buf).map(|read| Response::new(index, offset, read, buf));
            filler.finish(index, outcome);
        }
        completion
    }

    /// Reads exactly `buf.len()` bytes starting at `offset`.
    ///
    /// # Errors
    ///
    /// If the read fails, or if the file ends first. The second case is a real error here, unlike
    /// in [`Self::read_at`], because a caller who asked for an exact read said it knew the length.
    fn read_exact_at(&self, offset: u64, buf: &mut [u8]) -> Result<()> {
        let read = self.read_at(offset, buf)?;
        if read == buf.len() {
            Ok(())
        } else {
            Err(rudb_common::Error::io(format!(
                "wanted {} bytes at offset {offset} and the file had {read}",
                buf.len()
            )))
        }
    }

    /// Writes all of `data` starting at `offset`, extending the file if it has to.
    ///
    /// This does not make the write durable. Nothing is durable until [`Self::sync`] returns, and
    /// a write that has not been synced can be present, absent or reordered against another
    /// unsynced write after a crash. That is not a quirk of the simulation, it is what the
    /// hardware does, and it is the reason the simulation models it.
    ///
    /// # Errors
    ///
    /// If the underlying write fails, or if the file was not opened for writing.
    fn write_at(&self, offset: u64, data: &[u8]) -> Result<()>;

    /// Makes every write issued before this call durable.
    ///
    /// # Errors
    ///
    /// If the underlying sync fails. An error here is not recoverable by retrying, per the write
    /// handling discussion in `spec/11-transactions.md`: a failed `fsync` on Linux can drop the
    /// dirty pages, so a second call may return success while the data is gone.
    fn sync(&self) -> Result<()>;

    /// Cuts the file to `len` bytes, or extends it with zeroes.
    ///
    /// # Errors
    ///
    /// If the underlying truncate fails.
    fn truncate(&self, len: u64) -> Result<()>;

    /// How many bytes long the file currently is.
    ///
    /// # Errors
    ///
    /// If the length cannot be determined.
    fn len(&self) -> Result<u64>;

    /// Whether the file has no bytes in it.
    ///
    /// # Errors
    ///
    /// If the length cannot be determined.
    fn is_empty(&self) -> Result<bool> {
        Ok(self.len()? == 0)
    }
}

/// A place files live.
///
/// Object stores will implement this too, which is why there is no method that assumes a mutable
/// hierarchy beyond what a database actually needs. Rename is here because the atomic rename is how
/// a file gets replaced without a window where it is neither, and because an object store that
/// cannot do it needs to say so rather than have callers assume.
pub trait Filesystem: Debug + Send + Sync {
    /// Opens a file.
    ///
    /// # Errors
    ///
    /// If the file cannot be opened in the requested mode.
    fn open(&self, path: &Path, mode: OpenMode) -> Result<Box<dyn File>>;

    /// Whether a path exists.
    fn exists(&self, path: &Path) -> bool;

    /// Whether a path is a directory.
    ///
    /// Separate from [`Filesystem::exists`] because a pattern walk has to tell the two apart:
    /// `data/*` matches a directory and a file alike and only one of them can be read as a table.
    fn is_dir(&self, path: &Path) -> bool;

    /// What is directly inside a directory, as whole paths rather than as names.
    ///
    /// The order is whatever the filesystem gives, which is not an order. Anything that shows a
    /// caller more than one of these sorts them, because a directory's own layout differs between
    /// two machines holding the same files.
    ///
    /// # Errors
    ///
    /// If the directory cannot be read. A path that is not a directory is the caller's mistake and
    /// is an error here rather than an empty list.
    fn read_dir(&self, path: &Path) -> Result<Vec<PathBuf>>;

    /// Deletes a file.
    ///
    /// # Errors
    ///
    /// If the file cannot be deleted.
    fn remove(&self, path: &Path) -> Result<()>;

    /// Moves a file, replacing the destination if it exists.
    ///
    /// # Errors
    ///
    /// If the rename fails.
    fn rename(&self, from: &Path, to: &Path) -> Result<()>;

    /// Creates a directory and any missing parents.
    ///
    /// # Errors
    ///
    /// If the directory cannot be created.
    fn create_dir_all(&self, path: &Path) -> Result<()>;

    /// Makes a directory entry durable, which is what a rename needs before it counts.
    ///
    /// Easy to forget and it is the difference between a crash-safe atomic replace and one that
    /// works on every test and fails on a power cut. The rename itself being atomic says nothing
    /// about the directory entry having reached the disk.
    ///
    /// # Errors
    ///
    /// If the directory cannot be synced.
    fn sync_dir(&self, path: &Path) -> Result<()>;
}
