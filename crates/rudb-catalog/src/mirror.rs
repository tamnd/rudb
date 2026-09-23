//! Native mirrors of Parquet files, held beside the schemas rather than in them.
//!
//! A mirror is a native table holding exactly what `read_parquet` returns for one file, and the
//! binder reads it in place of the file when the file has not changed since the mirror was made.
//! Document 33 in `spec/storage-v3` is what one is and when it is trusted. What is here is the half
//! the binder needs: which files have one, and whether the file is still the file it was made from.
//!
//! Held apart from the schemas so that nothing that walks them sees one. A checkpoint writes the
//! schemas' tables into the user's file and `SHOW TABLES` lists them, and a mirror is neither the
//! user's data nor anything they created.

use std::path::Path;

use crate::name::QualifiedName;
use crate::table::Table;

/// The database name a mirror's table is found under.
///
/// Not a name a person can reach: the binder only ever produces it from a mirror it looked up, and
/// resolving a written name never looks at the mirrors.
pub const MIRROR_CATALOG: &str = "__rudb_mirror";

/// What the file system says about a file, which is everything that changes when it is written.
///
/// Compared on every read of a mirrored file, and it is one `stat`. The footer, which is what makes
/// a mirror's key hard to fool, is checked once when the mirror is opened and named by, so a file
/// whose stamp still matches is one whose footer was checked against the mirror.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FileStamp {
    pub device: u64,
    pub inode: u64,
    pub size: u64,
    /// Nanoseconds since the epoch, and zero where the platform will not say.
    pub modified: i128,
}

impl FileStamp {
    /// The stamp of the file at `path`, or `None` for one that is not a regular file.
    #[must_use]
    pub fn of(path: &Path) -> Option<Self> {
        let metadata = std::fs::metadata(path).ok()?;
        if !metadata.is_file() {
            return None;
        }
        let modified = metadata
            .modified()
            .ok()
            .and_then(|at| at.duration_since(std::time::UNIX_EPOCH).ok())
            .map_or(0, |since| since.as_nanos() as i128);
        let (device, inode) = identity(&metadata);
        Some(Self { device, inode, size: metadata.len(), modified })
    }
}

#[cfg(unix)]
fn identity(metadata: &std::fs::Metadata) -> (u64, u64) {
    use std::os::unix::fs::MetadataExt;
    (metadata.dev(), metadata.ino())
}

#[cfg(not(unix))]
fn identity(_: &std::fs::Metadata) -> (u64, u64) {
    (0, 0)
}

/// One mirrored file.
#[derive(Debug, Clone)]
pub(crate) struct Mirror {
    /// The file's canonical path.
    pub(crate) path: String,
    pub(crate) binary_as_string: bool,
    pub(crate) stamp: FileStamp,
    pub(crate) table: Table,
}

impl Mirror {
    /// The name the mirror at `at` in the list is found under.
    pub(crate) fn name(at: usize) -> QualifiedName {
        QualifiedName::new(MIRROR_CATALOG, "main", format!("m{at}"))
    }
}
