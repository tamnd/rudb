//! The filesystem that is an actual filesystem.
//!
//! Thin on purpose. Everything interesting about I/O in this project happens either above this,
//! in the buffer manager, or below it, in the kernel. What is here is the mapping from the
//! interface in the crate root onto positional reads and writes, and nothing else.
//!
//! Direct I/O and io_uring are not here. `spec/05-storage.md` says the layer ends up with two
//! backends chosen by measurement at startup, because io_uring is a win at queue depth and a loss
//! at low depth, and the Conviva result is the published case of somebody migrating to it and
//! getting slower. Choosing between them needs a buffer manager to generate the depth and a
//! workload to measure, and both arrive at M2.

use std::fs::{File as StdFile, OpenOptions};
use std::path::Path;

use rudb_common::{Error, Result};

use crate::{File, Filesystem, OpenMode};

/// Files on the machine this process is running on.
#[derive(Debug, Default, Clone, Copy)]
pub struct RealFilesystem;

impl RealFilesystem {
    /// A handle on the real filesystem.
    #[must_use]
    pub fn new() -> Self {
        Self
    }
}

impl Filesystem for RealFilesystem {
    fn open(&self, path: &Path, mode: OpenMode) -> Result<Box<dyn File>> {
        let mut options = OpenOptions::new();
        match mode {
            OpenMode::Read => {
                options.read(true);
            }
            OpenMode::ReadWrite => {
                options.read(true).write(true);
            }
            OpenMode::Create => {
                options.read(true).write(true).create(true);
            }
            OpenMode::CreateNew => {
                options.read(true).write(true).create_new(true);
            }
        }
        let file = options
            .open(path)
            .map_err(|e| Error::io(format!("could not open {}: {e}", path.display())))?;
        Ok(Box::new(RealFile { file, writable: mode.writable() }))
    }

    fn exists(&self, path: &Path) -> bool {
        path.exists()
    }

    fn remove(&self, path: &Path) -> Result<()> {
        std::fs::remove_file(path)
            .map_err(|e| Error::io(format!("could not remove {}: {e}", path.display())))
    }

    fn rename(&self, from: &Path, to: &Path) -> Result<()> {
        std::fs::rename(from, to).map_err(|e| {
            Error::io(format!("could not rename {} to {}: {e}", from.display(), to.display()))
        })
    }

    fn create_dir_all(&self, path: &Path) -> Result<()> {
        std::fs::create_dir_all(path)
            .map_err(|e| Error::io(format!("could not create {}: {e}", path.display())))
    }

    fn sync_dir(&self, path: &Path) -> Result<()> {
        // Windows has no notion of opening a directory to sync it, and its rename durability model
        // is different enough that pretending otherwise would be worse than saying so. The crash
        // tests run the simulation, which models the durability rules explicitly, so the platform
        // gap shows up there rather than being papered over here.
        #[cfg(windows)]
        {
            let _ = path;
            Ok(())
        }
        #[cfg(not(windows))]
        {
            let dir = StdFile::open(path)
                .map_err(|e| Error::io(format!("could not open {}: {e}", path.display())))?;
            dir.sync_all().map_err(|e| Error::io(format!("could not sync {}: {e}", path.display())))
        }
    }
}

/// An open file on the real filesystem.
#[derive(Debug)]
struct RealFile {
    file: StdFile,
    writable: bool,
}

impl File for RealFile {
    fn read_at(&self, offset: u64, buf: &mut [u8]) -> Result<usize> {
        #[cfg(unix)]
        let result = std::os::unix::fs::FileExt::read_at(&self.file, buf, offset);
        #[cfg(windows)]
        let result = std::os::windows::fs::FileExt::seek_read(&self.file, buf, offset);
        result.map_err(|e| Error::io(format!("read at {offset} failed: {e}")))
    }

    fn write_at(&self, offset: u64, data: &[u8]) -> Result<()> {
        if !self.writable {
            return Err(Error::io("this file was opened for reading"));
        }
        let mut written = 0usize;
        while written < data.len() {
            let at = offset + written as u64;
            let chunk = &data[written..];
            #[cfg(unix)]
            let result = std::os::unix::fs::FileExt::write_at(&self.file, chunk, at);
            #[cfg(windows)]
            let result = std::os::windows::fs::FileExt::seek_write(&self.file, chunk, at);
            let n = result.map_err(|e| Error::io(format!("write at {at} failed: {e}")))?;
            if n == 0 {
                return Err(Error::io(format!("write at {at} wrote nothing")));
            }
            written += n;
        }
        Ok(())
    }

    fn sync(&self) -> Result<()> {
        // `sync_all` and not `sync_data`. The difference is the metadata, and a file whose data is
        // durable while its length is not is a file that reads back short after a crash.
        self.file.sync_all().map_err(|e| Error::io(format!("sync failed: {e}")))
    }

    fn truncate(&self, len: u64) -> Result<()> {
        if !self.writable {
            return Err(Error::io("this file was opened for reading"));
        }
        self.file.set_len(len).map_err(|e| Error::io(format!("truncate to {len} failed: {e}")))
    }

    fn len(&self) -> Result<u64> {
        Ok(self.file.metadata().map_err(|e| Error::io(format!("stat failed: {e}")))?.len())
    }
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use super::RealFilesystem;
    use crate::submit::Request;
    use crate::{Filesystem, OpenMode};

    /// A directory under the system temporary directory that removes itself.
    ///
    /// Written out rather than pulled in, because a four line struct is cheaper than a dependency
    /// and `spec/18-package-layout.md` is strict about what the foundation crates are allowed to
    /// depend on.
    struct TempDir(PathBuf);

    impl TempDir {
        fn new(tag: &str) -> Self {
            let unique = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0);
            let path = std::env::temp_dir().join(format!("rudb-io-{tag}-{unique}"));
            std::fs::create_dir_all(&path).expect("could not make a temporary directory");
            Self(path)
        }

        fn join(&self, name: &str) -> PathBuf {
            self.0.join(name)
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn a_file_reads_back_what_was_written_at_the_offset_it_was_written_to() {
        let dir = TempDir::new("roundtrip");
        let fs = RealFilesystem::new();
        let path = dir.join("data");
        let file = fs.open(&path, OpenMode::CreateNew).unwrap();
        file.write_at(0, b"hello").unwrap();
        file.write_at(16, b"world").unwrap();
        file.sync().unwrap();

        let mut buf = [0u8; 5];
        file.read_exact_at(16, &mut buf).unwrap();
        assert_eq!(&buf, b"world");
        assert_eq!(file.len().unwrap(), 21);
        // The gap between the two writes is zeroes, not garbage, which is what makes a sparse
        // write safe to do and what a block allocator will rely on.
        let mut gap = [0xffu8; 11];
        file.read_exact_at(5, &mut gap).unwrap();
        assert_eq!(gap, [0u8; 11]);
    }

    #[test]
    fn create_new_refuses_to_open_a_file_that_is_already_there() {
        // "Create the database" and "open the database that is already there" are different
        // intentions, and collapsing them is how a process writes a header over somebody's data.
        let dir = TempDir::new("createnew");
        let fs = RealFilesystem::new();
        let path = dir.join("db");
        fs.open(&path, OpenMode::CreateNew).unwrap();
        assert!(fs.open(&path, OpenMode::CreateNew).is_err());
        assert!(fs.open(&path, OpenMode::Create).is_ok());
    }

    #[test]
    fn a_read_only_handle_refuses_to_write() {
        let dir = TempDir::new("readonly");
        let fs = RealFilesystem::new();
        let path = dir.join("data");
        fs.open(&path, OpenMode::CreateNew).unwrap().write_at(0, b"x").unwrap();
        let file = fs.open(&path, OpenMode::Read).unwrap();
        assert!(file.write_at(0, b"y").is_err());
        assert!(file.truncate(0).is_err());
    }

    #[test]
    fn a_short_read_at_the_end_is_a_short_read_and_an_exact_read_is_an_error() {
        let dir = TempDir::new("shortread");
        let fs = RealFilesystem::new();
        let path = dir.join("data");
        let file = fs.open(&path, OpenMode::CreateNew).unwrap();
        file.write_at(0, b"abc").unwrap();
        let mut buf = [0u8; 8];
        assert_eq!(file.read_at(0, &mut buf).unwrap(), 3);
        assert!(file.read_exact_at(0, &mut buf).is_err());
    }

    #[test]
    fn truncate_cuts_and_extends() {
        let dir = TempDir::new("truncate");
        let fs = RealFilesystem::new();
        let path = dir.join("data");
        let file = fs.open(&path, OpenMode::CreateNew).unwrap();
        file.write_at(0, b"abcdefgh").unwrap();
        file.truncate(3).unwrap();
        assert_eq!(file.len().unwrap(), 3);
        file.truncate(6).unwrap();
        let mut buf = [0xffu8; 6];
        file.read_exact_at(0, &mut buf).unwrap();
        assert_eq!(&buf, b"abc\0\0\0");
    }

    #[test]
    fn a_batch_of_reads_comes_back_answering_the_requests_it_was_given() {
        // The real filesystem takes the default `submit`, which is the loop over `read_at`. That
        // is still worth a test, because the default is what every backend has until somebody
        // writes it a queue, and a scan is written against `submit` from its first line.
        let dir = TempDir::new("submit");
        let fs = RealFilesystem::new();
        let path = dir.join("data");
        let file = fs.open(&path, OpenMode::CreateNew).unwrap();
        file.write_at(0, b"abcdefghijklmnop").unwrap();
        file.sync().unwrap();

        let responses = file
            .submit(vec![Request::new(8, 4), Request::new(0, 4), Request::new(12, 8)])
            .wait()
            .unwrap();
        assert_eq!(responses.len(), 3);
        assert_eq!(responses[0].bytes(), b"ijkl");
        assert_eq!(responses[1].bytes(), b"abcd");
        // The last one runs off the end of the file, which is a short read and not an error.
        assert!(responses[2].is_short());
        assert_eq!(responses[2].bytes(), b"mnop");
    }

    #[test]
    fn rename_replaces_and_remove_removes() {
        let dir = TempDir::new("rename");
        let fs = RealFilesystem::new();
        let from = dir.join("new");
        let to = dir.join("live");
        fs.open(&to, OpenMode::CreateNew).unwrap().write_at(0, b"old").unwrap();
        fs.open(&from, OpenMode::CreateNew).unwrap().write_at(0, b"new").unwrap();
        fs.rename(&from, &to).unwrap();
        assert!(!fs.exists(&from));

        let mut buf = [0u8; 3];
        fs.open(&to, OpenMode::Read).unwrap().read_exact_at(0, &mut buf).unwrap();
        assert_eq!(&buf, b"new");

        fs.remove(&to).unwrap();
        assert!(!fs.exists(&to));
    }
}
