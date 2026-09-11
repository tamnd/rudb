//! The interception shim.
//!
//! `spec/16-testing.md` section 16.5 specifies this: every write, fsync, rename and truncate goes
//! through a shim that records it and can be told to fail at a chosen point, and to reorder writes
//! that were not separated by an fsync. The crash consistency tests that drive it are M6 work. The
//! shim is M0 work because retrofitting it into a codebase that has spent two years calling
//! `File::write` is a much larger job than building against it from the start.
//!
//! # The durability model
//!
//! A write goes into a pending list. A read sees it immediately, because that is what the process
//! sees: the page cache serves the read whether or not the data has reached the disk. A [`sync`]
//! moves everything pending for that file into the durable image.
//!
//! [`SimFilesystem::crash`] then produces a new filesystem holding the durable image plus whichever
//! subset of the pending writes the caller says survived. That subset is the whole point. A crash
//! after two writes with no fsync between them can leave either, both or neither, and section 16.5
//! is specific that testing only the truncation case misses the bugs that actually happen on real
//! hardware. The subsets are enumerated by the test, exhaustively, which is what lets a crash test
//! prove something rather than merely fail to find a bug.
//!
//! # What this does not model yet
//!
//! Directory entry durability. A rename here takes effect immediately, where on a real filesystem
//! the rename is atomic but the directory entry reaching the disk is a separate question that
//! [`Filesystem::sync_dir`] answers. Modelling that means a pending list per directory as well as
//! per file, and it is the difference between testing the atomic replace pattern properly and
//! testing most of it. It is tracked as issue #19 rather than left as a surprise, and it is needed
//! before the M6 crash tests can claim to cover the header swap in `spec/05-storage.md`.
//!
//! Torn writes within a single `write_at` are not modelled either. A 4 KiB write is atomic on
//! essentially every device this will run on, and a larger one is decomposed by the caller into
//! block sized writes, so the interesting reordering is between writes rather than inside one.
//!
//! # The read side
//!
//! Writes were the whole story while the only thing above this layer was a writer. A reader arrived
//! at M2d and it has its own three failures, listed in the test gate on the sub-milestone issue:
//! short reads, reordered completions and an error part way through a batch. All three are
//! injectable here, through [`SimFilesystem::short_read_at`], [`SimFilesystem::fail_read_at`] and
//! [`SimFilesystem::complete`], and all three are deterministic, which is the point of doing it
//! here rather than by unplugging a disk.
//!
//! Reads are not in the operation log and do not move the failure point indices, so a crash test
//! written before any of this still enumerates the same points. They are counted separately, by
//! [`SimFilesystem::reads_served`], and the read faults are addressed by that counter.
//!
//! [`sync`]: crate::File::sync

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use rudb_common::{Error, Result};

use crate::submit::{Completion, Request, Response};
use crate::{File, Filesystem, OpenMode};

/// One recorded operation.
///
/// Writes record their length rather than their bytes, because a log of a ClickBench load with the
/// payloads in it is a log nobody can read and a test nobody can debug.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Op {
    /// A file was opened.
    Open {
        /// The file.
        path: PathBuf,
        /// How it was opened.
        mode: OpenMode,
    },
    /// Bytes were written.
    Write {
        /// The file.
        path: PathBuf,
        /// Where they went.
        offset: u64,
        /// How many.
        len: usize,
    },
    /// A file was made durable.
    Sync {
        /// The file.
        path: PathBuf,
    },
    /// A file was cut or extended.
    Truncate {
        /// The file.
        path: PathBuf,
        /// The new length.
        len: u64,
    },
    /// A file was moved.
    Rename {
        /// Where it was.
        from: PathBuf,
        /// Where it went.
        to: PathBuf,
    },
    /// A file was deleted.
    Remove {
        /// The file.
        path: PathBuf,
    },
    /// A directory was created.
    CreateDir {
        /// The directory.
        path: PathBuf,
    },
    /// A directory entry was made durable.
    SyncDir {
        /// The directory.
        path: PathBuf,
    },
}

impl Op {
    /// Whether this operation is one that makes earlier work durable.
    ///
    /// The failure point enumeration in section 16.5 cares about these, because the interval
    /// between two of them is the window in which writes can be reordered against each other.
    #[must_use]
    pub fn is_durability_point(&self) -> bool {
        matches!(self, Self::Sync { .. } | Self::SyncDir { .. })
    }
}

/// Which unsynced writes survived a crash.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Crash {
    /// None of them. The pessimistic case, and the one most tests reach for first.
    LosingUnsynced,
    /// All of them. Not a crash so much as a clean shutdown, and worth testing because a database
    /// that only works when writes are lost has a different bug.
    KeepingEverything,
    /// Exactly these, by the sequence numbers from [`SimFilesystem::pending`].
    ///
    /// This is the variant the exhaustive enumeration uses. With three unsynced writes there are
    /// eight subsets and the test runs all eight.
    Keeping(Vec<u64>),
}

impl Crash {
    fn keeps(&self, seq: u64) -> bool {
        match self {
            Self::LosingUnsynced => false,
            Self::KeepingEverything => true,
            Self::Keeping(kept) => kept.contains(&seq),
        }
    }
}

/// A write that has been issued and not yet made durable.
#[derive(Debug, Clone)]
struct Pending {
    seq: u64,
    change: Change,
}

#[derive(Debug, Clone)]
enum Change {
    Write { offset: u64, data: Vec<u8> },
    Truncate { len: u64 },
}

#[derive(Debug, Clone, Default)]
struct SimFile {
    /// What is on the disk.
    durable: Vec<u8>,
    /// What has been written and not synced, in the order it was issued.
    pending: Vec<Pending>,
}

impl SimFile {
    /// What a reader sees, which is the durable image with everything pending applied.
    fn visible(&self) -> Vec<u8> {
        let mut bytes = self.durable.clone();
        for entry in &self.pending {
            apply(&mut bytes, &entry.change);
        }
        bytes
    }
}

fn apply(bytes: &mut Vec<u8>, change: &Change) {
    match change {
        Change::Write { offset, data } => {
            let end = *offset as usize + data.len();
            if bytes.len() < end {
                bytes.resize(end, 0);
            }
            bytes[*offset as usize..end].copy_from_slice(data);
        }
        Change::Truncate { len } => bytes.resize(*len as usize, 0),
    }
}

/// The order a batch handed to `submit` comes back in.
///
/// A reader that only ever works because the answers arrived in the order it asked for them is a
/// reader that works on a warm page cache and breaks on the first machine where two reads take
/// different amounts of time. Which is every machine.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Completions {
    /// In submission order, which is the boring case and the default.
    #[default]
    InOrder,
    /// Last submitted first. Cheap, deterministic, and it catches a caller that reads the first
    /// response before checking which request it answers.
    Reversed,
    /// A deterministic shuffle from this seed, so a failing run is rerunnable from the seed alone.
    Shuffled(u64),
}

/// What the simulation has been told to do to the reads.
#[derive(Debug, Default)]
struct ReadFaults {
    /// How many reads have been served since this filesystem was made. The faults below are
    /// addressed by this number, so a test says which read it wants to break rather than having to
    /// reach the file handle that will serve it.
    served: u64,
    /// Reads that come back with fewer bytes than were asked for, and how many bytes they give.
    short: BTreeMap<u64, usize>,
    /// Reads that come back as an error.
    failing: BTreeSet<u64>,
    /// The order a submitted batch is completed in.
    order: Completions,
}

#[derive(Debug, Default)]
struct Inner {
    files: BTreeMap<PathBuf, SimFile>,
    dirs: BTreeSet<PathBuf>,
    log: Vec<Op>,
    next_seq: u64,
    /// The index in the log at which one operation is made to fail.
    fail_at: Option<usize>,
    reads: ReadFaults,
}

impl Inner {
    /// Records an operation and says whether the injected failure lands on this one.
    ///
    /// The operation is recorded either way. A failure that leaves no trace in the log is a
    /// failure the test cannot find its way back to.
    fn record(&mut self, op: Op) -> Result<()> {
        let index = self.log.len();
        self.log.push(op);
        if self.fail_at == Some(index) {
            self.fail_at = None;
            return Err(Error::io(format!("injected failure at operation {index}")));
        }
        Ok(())
    }

    /// Serves one read, applying whatever fault was addressed at this read.
    ///
    /// The read is counted before anything else can go wrong with it, so an injected failure on
    /// read seven does not shift what read eight is.
    fn serve_read(&mut self, path: &Path, offset: u64, buf: &mut [u8]) -> Result<usize> {
        let number = self.reads.served;
        self.reads.served += 1;
        let failing = self.reads.failing.remove(&number);
        let short = self.reads.short.remove(&number);
        if failing {
            return Err(Error::io(format!("injected read failure on read {number}")));
        }
        let file = self
            .files
            .get(path)
            .ok_or_else(|| Error::io(format!("{} was removed while open", path.display())))?;
        let bytes = file.visible();
        let start = offset as usize;
        if start >= bytes.len() {
            return Ok(0);
        }
        let mut n = buf.len().min(bytes.len() - start);
        if let Some(cap) = short {
            n = n.min(cap);
        }
        buf[..n].copy_from_slice(&bytes[start..start + n]);
        Ok(n)
    }
}

/// Permutes a batch of finished reads into the order the simulation says they came back in.
///
/// The reads themselves are served in submission order whatever this says, so that the numbering
/// the read faults are addressed by does not depend on the completion order. Only the order the
/// caller hears about them in changes, which is the thing being tested.
fn reorder<T>(outcomes: &mut [T], order: Completions) {
    match order {
        Completions::InOrder => {}
        Completions::Reversed => outcomes.reverse(),
        Completions::Shuffled(seed) => {
            // SplitMix64, written out because it is nine lines and the workspace has no
            // dependencies. Any deterministic generator would do; this one is the one with the
            // shortest description that passes the tests people run on generators.
            let mut state = seed;
            let mut next = move || {
                state = state.wrapping_add(0x9e37_79b9_7f4a_7c15);
                let mut z = state;
                z = (z ^ (z >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
                z = (z ^ (z >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
                z ^ (z >> 31)
            };
            for i in (1..outcomes.len()).rev() {
                let j = (next() % (i as u64 + 1)) as usize;
                outcomes.swap(i, j);
            }
        }
    }
}

/// A filesystem in memory that records what was done to it.
///
/// Cloning one gives another handle on the same filesystem, not a copy of it, which is what makes
/// it usable in the places a real one would be shared.
#[derive(Debug, Clone, Default)]
pub struct SimFilesystem {
    inner: Arc<Mutex<Inner>>,
}

impl SimFilesystem {
    /// An empty filesystem.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Inner> {
        // A poisoned mutex means a test panicked while holding it, and the panic is the finding.
        // Unwrapping the poison rather than propagating it keeps that panic as the reported
        // failure instead of burying it under a lock error from an unrelated assertion.
        self.inner.lock().unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// Everything that has been done to this filesystem, in order.
    #[must_use]
    pub fn ops(&self) -> Vec<Op> {
        self.lock().log.clone()
    }

    /// How many operations have been recorded.
    ///
    /// This is the count the failure point enumeration in section 16.5 runs over: record a
    /// workload, then rerun it once per index with the failure injected there.
    #[must_use]
    pub fn op_count(&self) -> usize {
        self.lock().log.len()
    }

    /// Forgets the recorded operations, keeping the contents.
    ///
    /// For a test that has to set up a database and only wants to enumerate failure points over
    /// what comes after the setup.
    pub fn clear_log(&self) {
        self.lock().log.clear();
    }

    /// Makes the operation at `index` fail once.
    ///
    /// This is error path testing and it is a different mechanism from [`Self::crash`], on purpose.
    /// An `EIO` on one write is a thing the caller has to handle and keep running from. A crash is
    /// a thing the caller does not get to handle at all, and the question there is what the next
    /// process to open the file sees. Conflating them produces a test that proves neither.
    pub fn fail_at(&self, index: usize) {
        self.lock().fail_at = Some(index);
    }

    /// Cancels an injected failure that has not fired.
    pub fn clear_failure(&self) {
        self.lock().fail_at = None;
    }

    /// How many reads have been served since this filesystem was made.
    ///
    /// This is the number the read faults below are addressed by, and it is also the byte counting
    /// hook's coarser sibling: a reader that reads a row group it should have pruned makes more
    /// reads than one that does not, and the count says so whatever the answer was.
    #[must_use]
    pub fn reads_served(&self) -> u64 {
        self.lock().reads.served
    }

    /// Makes read number `read` come back with `len` bytes rather than the length asked for.
    ///
    /// A short read is not an error, it is a short read, and the reason it is worth injecting is
    /// that a decoder which treats a returned buffer as full reads whatever was in the buffer
    /// before, which is a wrong answer and not a crash.
    pub fn short_read_at(&self, read: u64, len: usize) {
        self.lock().reads.short.insert(read, len);
    }

    /// Makes read number `read` fail.
    ///
    /// Once, like [`Self::fail_at`], and for the same reason: the interesting question is whether
    /// the caller survives one failure, not whether it survives a disk that is gone.
    pub fn fail_read_at(&self, read: u64) {
        self.lock().reads.failing.insert(read);
    }

    /// Sets the order a batch handed to `submit` comes back in.
    pub fn complete(&self, order: Completions) {
        self.lock().reads.order = order;
    }

    /// Cancels every injected read fault and puts completions back in submission order.
    pub fn clear_read_faults(&self) {
        let mut inner = self.lock();
        inner.reads.short.clear();
        inner.reads.failing.clear();
        inner.reads.order = Completions::InOrder;
    }

    /// The writes that have been issued and not made durable, as sequence numbers with their file.
    ///
    /// These are the numbers [`Crash::Keeping`] takes. The order is the order they were issued in,
    /// across all files, because two writes to different files race with each other exactly the way
    /// two writes to one file do.
    #[must_use]
    pub fn pending(&self) -> Vec<(u64, PathBuf)> {
        let inner = self.lock();
        let mut out: Vec<(u64, PathBuf)> = inner
            .files
            .iter()
            .flat_map(|(path, file)| file.pending.iter().map(|p| (p.seq, path.clone())))
            .collect();
        out.sort_by_key(|(seq, _)| *seq);
        out
    }

    /// The filesystem a process would find after a crash.
    ///
    /// The durable image, plus whichever pending writes `crash` says survived, applied in the order
    /// they were issued. The result is a fresh filesystem with an empty log, because the log
    /// belongs to the process that died.
    #[must_use]
    pub fn crash(&self, crash: &Crash) -> Self {
        let inner = self.lock();
        let mut files = BTreeMap::new();
        for (path, file) in &inner.files {
            let mut bytes = file.durable.clone();
            for entry in &file.pending {
                if crash.keeps(entry.seq) {
                    apply(&mut bytes, &entry.change);
                }
            }
            files.insert(path.clone(), SimFile { durable: bytes, pending: Vec::new() });
        }
        Self {
            inner: Arc::new(Mutex::new(Inner {
                files,
                dirs: inner.dirs.clone(),
                log: Vec::new(),
                next_seq: 0,
                fail_at: None,
                reads: ReadFaults::default(),
            })),
        }
    }

    /// The durable contents of a file, ignoring anything unsynced.
    ///
    /// For a test that wants to assert what is on the disk without going through a crash first.
    #[must_use]
    pub fn durable_contents(&self, path: &Path) -> Option<Vec<u8>> {
        self.lock().files.get(path).map(|file| file.durable.clone())
    }

    /// The contents a reader would see right now, unsynced writes included.
    #[must_use]
    pub fn contents(&self, path: &Path) -> Option<Vec<u8>> {
        self.lock().files.get(path).map(SimFile::visible)
    }
}

impl Filesystem for SimFilesystem {
    fn open(&self, path: &Path, mode: OpenMode) -> Result<Box<dyn File>> {
        let mut inner = self.lock();
        let exists = inner.files.contains_key(path);
        match mode {
            OpenMode::Read | OpenMode::ReadWrite if !exists => {
                // Recorded before the error, so that a test enumerating failure points sees the
                // same log whether or not this open succeeded.
                inner.record(Op::Open { path: path.to_path_buf(), mode })?;
                return Err(Error::io(format!("{} does not exist", path.display())));
            }
            OpenMode::CreateNew if exists => {
                inner.record(Op::Open { path: path.to_path_buf(), mode })?;
                return Err(Error::io(format!("{} already exists", path.display())));
            }
            _ => {}
        }
        inner.record(Op::Open { path: path.to_path_buf(), mode })?;
        inner.files.entry(path.to_path_buf()).or_default();
        Ok(Box::new(SimHandle {
            fs: self.clone(),
            path: path.to_path_buf(),
            writable: mode.writable(),
        }))
    }

    fn exists(&self, path: &Path) -> bool {
        let inner = self.lock();
        inner.files.contains_key(path) || inner.dirs.contains(path)
    }

    fn remove(&self, path: &Path) -> Result<()> {
        let mut inner = self.lock();
        inner.record(Op::Remove { path: path.to_path_buf() })?;
        if inner.files.remove(path).is_none() {
            return Err(Error::io(format!("{} does not exist", path.display())));
        }
        Ok(())
    }

    fn rename(&self, from: &Path, to: &Path) -> Result<()> {
        let mut inner = self.lock();
        inner.record(Op::Rename { from: from.to_path_buf(), to: to.to_path_buf() })?;
        let Some(file) = inner.files.remove(from) else {
            return Err(Error::io(format!("{} does not exist", from.display())));
        };
        inner.files.insert(to.to_path_buf(), file);
        Ok(())
    }

    fn create_dir_all(&self, path: &Path) -> Result<()> {
        let mut inner = self.lock();
        inner.record(Op::CreateDir { path: path.to_path_buf() })?;
        let mut current = PathBuf::new();
        for part in path {
            current.push(part);
            inner.dirs.insert(current.clone());
        }
        Ok(())
    }

    fn sync_dir(&self, path: &Path) -> Result<()> {
        let mut inner = self.lock();
        inner.record(Op::SyncDir { path: path.to_path_buf() })
    }
}

/// An open file on a [`SimFilesystem`].
#[derive(Debug)]
struct SimHandle {
    fs: SimFilesystem,
    path: PathBuf,
    writable: bool,
}

impl SimHandle {
    fn missing(&self) -> Error {
        Error::io(format!("{} was removed while open", self.path.display()))
    }
}

impl File for SimHandle {
    fn read_at(&self, offset: u64, buf: &mut [u8]) -> Result<usize> {
        self.fs.lock().serve_read(&self.path, offset, buf)
    }

    fn submit(&self, requests: Vec<Request>) -> Completion {
        let (completion, filler) = Completion::pending(requests.len());
        let mut outcomes = Vec::with_capacity(requests.len());
        for (index, request) in requests.into_iter().enumerate() {
            let offset = request.offset();
            let mut buf = request.into_buffer();
            let outcome =
                self.read_at(offset, &mut buf).map(|read| Response::new(index, offset, read, buf));
            outcomes.push((index, outcome));
        }
        let order = self.fs.lock().reads.order;
        reorder(&mut outcomes, order);
        for (index, outcome) in outcomes {
            filler.finish(index, outcome);
        }
        completion
    }

    fn write_at(&self, offset: u64, data: &[u8]) -> Result<()> {
        if !self.writable {
            return Err(Error::io("this file was opened for reading"));
        }
        let mut inner = self.fs.lock();
        inner.record(Op::Write { path: self.path.clone(), offset, len: data.len() })?;
        let seq = inner.next_seq;
        inner.next_seq += 1;
        let file = inner.files.get_mut(&self.path).ok_or_else(|| self.missing())?;
        file.pending.push(Pending { seq, change: Change::Write { offset, data: data.to_vec() } });
        Ok(())
    }

    fn sync(&self) -> Result<()> {
        let mut inner = self.fs.lock();
        inner.record(Op::Sync { path: self.path.clone() })?;
        let file = inner.files.get_mut(&self.path).ok_or_else(|| self.missing())?;
        let pending = std::mem::take(&mut file.pending);
        let mut durable = std::mem::take(&mut file.durable);
        for entry in &pending {
            apply(&mut durable, &entry.change);
        }
        file.durable = durable;
        Ok(())
    }

    fn truncate(&self, len: u64) -> Result<()> {
        if !self.writable {
            return Err(Error::io("this file was opened for reading"));
        }
        let mut inner = self.fs.lock();
        inner.record(Op::Truncate { path: self.path.clone(), len })?;
        let seq = inner.next_seq;
        inner.next_seq += 1;
        let file = inner.files.get_mut(&self.path).ok_or_else(|| self.missing())?;
        file.pending.push(Pending { seq, change: Change::Truncate { len } });
        Ok(())
    }

    fn len(&self) -> Result<u64> {
        let inner = self.fs.lock();
        let file = inner.files.get(&self.path).ok_or_else(|| self.missing())?;
        Ok(file.visible().len() as u64)
    }
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use super::{Completions, Crash, Op, SimFilesystem};
    use crate::submit::{Request, Response};
    use crate::{Filesystem, OpenMode};

    fn write_two_unsynced(fs: &SimFilesystem) {
        let file = fs.open(Path::new("/db"), OpenMode::Create).unwrap();
        file.write_at(0, b"AAAA").unwrap();
        file.sync().unwrap();
        file.write_at(0, b"BBBB").unwrap();
        file.write_at(4, b"CCCC").unwrap();
    }

    #[test]
    fn a_reader_sees_a_write_before_it_is_durable() {
        // Because that is what the process sees. The page cache serves the read whether or not the
        // data reached the disk, and a shim that pretended otherwise would make every test pass
        // for a reason that has nothing to do with the disk.
        let fs = SimFilesystem::new();
        let file = fs.open(Path::new("/db"), OpenMode::Create).unwrap();
        file.write_at(0, b"hello").unwrap();
        let mut buf = [0u8; 5];
        file.read_exact_at(0, &mut buf).unwrap();
        assert_eq!(&buf, b"hello");
        assert_eq!(fs.durable_contents(Path::new("/db")).unwrap(), Vec::<u8>::new());
    }

    #[test]
    fn a_crash_can_leave_either_both_or_neither() {
        // The case section 16.5 is specific about. Two writes with no fsync between them, and all
        // four outcomes are real. A crash test that only checks the truncation case misses the
        // bugs that actually happen on hardware.
        let mut seen = Vec::new();
        for kept in [vec![], vec![1], vec![2], vec![1, 2]] {
            let fs = SimFilesystem::new();
            write_two_unsynced(&fs);
            let pending = fs.pending();
            assert_eq!(pending.len(), 2, "both writes are unsynced");
            let after = fs.crash(&Crash::Keeping(kept.clone()));
            seen.push(after.durable_contents(Path::new("/db")).unwrap());
        }
        assert_eq!(seen[0], b"AAAA".to_vec(), "neither landed");
        assert_eq!(seen[1], b"BBBB".to_vec(), "the first landed");
        assert_eq!(seen[2], b"AAAACCCC".to_vec(), "the second landed and left a hole of zeroes");
        assert_eq!(seen[3], b"BBBBCCCC".to_vec(), "both landed");
    }

    #[test]
    fn everything_before_a_sync_survives_a_crash() {
        let fs = SimFilesystem::new();
        write_two_unsynced(&fs);
        let after = fs.crash(&Crash::LosingUnsynced);
        assert_eq!(after.durable_contents(Path::new("/db")).unwrap(), b"AAAA".to_vec());
        assert!(after.pending().is_empty(), "a crashed filesystem has nothing in flight");
        assert_eq!(after.op_count(), 0, "the log belonged to the process that died");
    }

    #[test]
    fn keeping_everything_is_what_a_clean_shutdown_looks_like() {
        let fs = SimFilesystem::new();
        write_two_unsynced(&fs);
        let after = fs.crash(&Crash::KeepingEverything);
        assert_eq!(after.durable_contents(Path::new("/db")).unwrap(), b"BBBBCCCC".to_vec());
    }

    #[test]
    fn every_operation_is_recorded_in_order() {
        let fs = SimFilesystem::new();
        fs.create_dir_all(Path::new("/data")).unwrap();
        let file = fs.open(Path::new("/data/db"), OpenMode::CreateNew).unwrap();
        file.write_at(0, b"xyz").unwrap();
        file.truncate(2).unwrap();
        file.sync().unwrap();
        fs.rename(Path::new("/data/db"), Path::new("/data/live")).unwrap();
        fs.sync_dir(Path::new("/data")).unwrap();
        fs.remove(Path::new("/data/live")).unwrap();

        let ops = fs.ops();
        assert!(matches!(ops[0], Op::CreateDir { .. }));
        assert!(matches!(ops[1], Op::Open { .. }));
        assert_eq!(ops[2], Op::Write { path: "/data/db".into(), offset: 0, len: 3 });
        assert_eq!(ops[3], Op::Truncate { path: "/data/db".into(), len: 2 });
        assert!(ops[4].is_durability_point());
        assert!(matches!(ops[5], Op::Rename { .. }));
        assert!(ops[6].is_durability_point());
        assert!(matches!(ops[7], Op::Remove { .. }));
        assert_eq!(fs.op_count(), 8);
    }

    #[test]
    fn an_injected_failure_hits_the_operation_it_was_aimed_at() {
        let fs = SimFilesystem::new();
        let file = fs.open(Path::new("/db"), OpenMode::Create).unwrap();
        // Operation 0 was the open. Aim at operation 2, which is the second write.
        fs.fail_at(2);
        assert!(file.write_at(0, b"a").is_ok());
        assert!(file.write_at(1, b"b").is_err());
        assert!(file.write_at(1, b"c").is_ok(), "one shot, not a permanently broken disk");
        // The failed write left no data behind but is in the log, because a test that enumerates
        // failure points has to be able to find its way back to the point it injected.
        assert_eq!(fs.contents(Path::new("/db")).unwrap(), b"ac".to_vec());
        assert_eq!(fs.op_count(), 4);
    }

    #[test]
    fn a_failed_sync_leaves_the_writes_unsynced() {
        // Which is the conservative model. A failed fsync on Linux can drop the dirty pages, so
        // the one thing a caller must not conclude is that the data is safe.
        let fs = SimFilesystem::new();
        let file = fs.open(Path::new("/db"), OpenMode::Create).unwrap();
        file.write_at(0, b"data").unwrap();
        fs.fail_at(2);
        assert!(file.sync().is_err());
        assert_eq!(fs.durable_contents(Path::new("/db")).unwrap(), Vec::<u8>::new());
        assert_eq!(fs.pending().len(), 1);
    }

    #[test]
    fn the_enumeration_the_crash_tests_will_run_is_expressible_today() {
        // Not a crash test, since there is no database to be consistent yet. This is the shape of
        // the loop from section 16.5, run against the shim itself, so that the apparatus is known
        // to work before something depends on it being right.
        let fs = SimFilesystem::new();
        let file = fs.open(Path::new("/db"), OpenMode::Create).unwrap();
        file.write_at(0, b"1").unwrap();
        file.write_at(1, b"2").unwrap();
        file.write_at(2, b"3").unwrap();
        let seqs: Vec<u64> = fs.pending().into_iter().map(|(seq, _)| seq).collect();
        assert_eq!(seqs.len(), 3);

        let mut outcomes = std::collections::BTreeSet::new();
        for mask in 0u32..(1 << seqs.len()) {
            let kept: Vec<u64> = seqs
                .iter()
                .enumerate()
                .filter(|(bit, _)| mask & (1 << bit) != 0)
                .map(|(_, seq)| *seq)
                .collect();
            let after = fs.crash(&Crash::Keeping(kept));
            outcomes.insert(after.durable_contents(Path::new("/db")).unwrap());
        }
        // Eight subsets, eight distinct files, because each write covers a different byte. A
        // scheme where two subsets produced the same bytes would be one where the enumeration was
        // doing less work than it looks like.
        assert_eq!(outcomes.len(), 8);
    }

    #[test]
    fn a_handle_on_a_removed_file_reports_that_rather_than_pretending() {
        let fs = SimFilesystem::new();
        let file = fs.open(Path::new("/db"), OpenMode::Create).unwrap();
        file.write_at(0, b"x").unwrap();
        fs.remove(Path::new("/db")).unwrap();
        assert!(file.len().is_err());
        assert!(file.write_at(0, b"y").is_err());
    }

    #[test]
    fn opening_a_file_that_is_not_there_fails_and_creating_one_that_is_fails_too() {
        let fs = SimFilesystem::new();
        assert!(fs.open(Path::new("/nope"), OpenMode::Read).is_err());
        assert!(fs.open(Path::new("/nope"), OpenMode::ReadWrite).is_err());
        fs.open(Path::new("/db"), OpenMode::CreateNew).unwrap();
        assert!(fs.open(Path::new("/db"), OpenMode::CreateNew).is_err());
        assert!(fs.open(Path::new("/db"), OpenMode::Create).is_ok());
    }

    #[test]
    fn clearing_the_log_keeps_the_contents() {
        let fs = SimFilesystem::new();
        let file = fs.open(Path::new("/db"), OpenMode::Create).unwrap();
        file.write_at(0, b"kept").unwrap();
        file.sync().unwrap();
        fs.clear_log();
        assert_eq!(fs.op_count(), 0);
        assert_eq!(fs.durable_contents(Path::new("/db")).unwrap(), b"kept".to_vec());
    }

    /// Sixteen bytes of `abcdefghijklmnop`, which is short enough to read in an assertion.
    fn alphabet(fs: &SimFilesystem) -> Box<dyn crate::File> {
        let file = fs.open(Path::new("/data"), OpenMode::Create).unwrap();
        file.write_at(0, b"abcdefghijklmnop").unwrap();
        file.sync().unwrap();
        file
    }

    #[test]
    fn a_submitted_batch_comes_back_whole_and_in_submission_order() {
        let fs = SimFilesystem::new();
        let file = alphabet(&fs);
        let responses = file
            .submit(vec![Request::new(0, 4), Request::new(8, 4), Request::new(4, 4)])
            .wait()
            .unwrap();
        let bytes: Vec<&[u8]> = responses.iter().map(Response::bytes).collect();
        assert_eq!(bytes, [b"abcd", b"ijkl", b"efgh"]);
        assert_eq!(fs.reads_served(), 3);
    }

    #[test]
    fn reordered_completions_still_say_which_request_they_answer() {
        // The failure this is aimed at is a caller that takes the first response to arrive and
        // assumes it is the first page it asked for. In order the bug is invisible.
        let fs = SimFilesystem::new();
        let file = alphabet(&fs);
        fs.complete(Completions::Reversed);
        let mut completion = file.submit(vec![Request::new(0, 4), Request::new(4, 4)]);
        let first = completion.take().unwrap().unwrap();
        assert_eq!(first.index(), 1);
        assert_eq!(first.bytes(), b"efgh");
        assert_eq!(completion.take().unwrap().unwrap().index(), 0);
        assert!(completion.take().is_none());
    }

    #[test]
    fn a_shuffle_is_the_same_shuffle_every_time_for_a_seed() {
        let order = |seed| {
            let fs = SimFilesystem::new();
            let file = alphabet(&fs);
            fs.complete(Completions::Shuffled(seed));
            let mut completion =
                file.submit((0..8).map(|i| Request::new(i * 2, 2)).collect::<Vec<_>>());
            let mut seen = Vec::new();
            while let Some(response) = completion.take() {
                seen.push(response.unwrap().index());
            }
            seen
        };
        assert_eq!(order(7), order(7), "the same seed is the same run");
        assert_ne!(order(7), order(8), "and a different one is a different run");
        let mut sorted = order(7);
        sorted.sort_unstable();
        assert_eq!(sorted, (0..8).collect::<Vec<_>>(), "every request is answered exactly once");
    }

    #[test]
    fn an_injected_short_read_is_short_and_is_not_an_error() {
        let fs = SimFilesystem::new();
        let file = alphabet(&fs);
        fs.short_read_at(1, 2);
        let responses = file.submit(vec![Request::new(0, 4), Request::new(4, 4)]).wait().unwrap();
        assert!(!responses[0].is_short());
        assert!(responses[1].is_short());
        assert_eq!(responses[1].bytes(), b"ef");
        // Once. The next read at the same offset is whole again, because the question is whether
        // the caller survives one short read rather than whether it survives a broken disk.
        assert_eq!(file.read_at(4, &mut [0u8; 4]).unwrap(), 4);
    }

    #[test]
    fn an_error_part_way_through_a_batch_leaves_the_rest_of_the_batch_alone() {
        let fs = SimFilesystem::new();
        let file = alphabet(&fs);
        fs.fail_read_at(1);
        let mut completion =
            file.submit(vec![Request::new(0, 4), Request::new(4, 4), Request::new(8, 4)]);
        let mut answered = 0;
        let mut failed = 0;
        while let Some(outcome) = completion.take() {
            match outcome {
                Ok(_) => answered += 1,
                Err(_) => failed += 1,
            }
        }
        assert_eq!((answered, failed), (2, 1));
    }

    #[test]
    fn a_failed_read_does_not_shift_which_read_the_next_fault_lands_on() {
        let fs = SimFilesystem::new();
        let file = alphabet(&fs);
        fs.fail_read_at(0);
        fs.short_read_at(1, 1);
        let responses = file.submit(vec![Request::new(0, 4), Request::new(4, 4)]);
        let mut outcomes = responses;
        let first = outcomes.take().unwrap();
        let second = outcomes.take().unwrap();
        assert!(first.is_err());
        assert_eq!(second.unwrap().bytes(), b"e");
    }

    #[test]
    fn read_at_and_a_batch_of_one_are_the_same_read() {
        let fs = SimFilesystem::new();
        let file = alphabet(&fs);
        let mut buf = [0u8; 5];
        file.read_exact_at(3, &mut buf).unwrap();
        let batched = file.submit(vec![Request::new(3, 5)]).wait().unwrap();
        assert_eq!(batched[0].bytes(), &buf);
    }
}
