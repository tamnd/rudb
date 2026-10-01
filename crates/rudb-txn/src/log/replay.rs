//! Reading a lane back after a restart, `09-the-log.md` section 9.8.
//!
//! Segments are read in sequence order and each from its first record. The first record that does
//! not verify ends its segment: past it are zeros, a block that was being written when the process
//! stopped, or, in a recycled segment, records of an older sequence that fail under this one's
//! seed. Replay goes on with the next segment, because the lane synced this one before it wrote a
//! byte to that one. Records after a segment's last Commit belong to a block that never finished
//! and are dropped, and nobody was told that block committed.

use std::ops::{Deref, Range};
use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use rudb_common::{Error, Result};
use rudb_io::{Filesystem, Mapped, OpenMode};

use super::format::{
    Commit, Kind, RECORD_HEADER, RecordHeader, SEGMENT_HEADER, SegmentHeader, decode_record, seed,
};
use super::lane::segments;

/// One record of a committed block.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Record {
    /// Its header.
    pub header: RecordHeader,
    /// Its payload.
    pub payload: Payload,
}

/// A record's payload, held as a range of the segment it was read from so that replay does not
/// copy every record out of the file's bytes. The segment stays in memory while any of its
/// payloads do.
#[derive(Clone)]
pub struct Payload {
    segment: Arc<Segment>,
    range: Range<usize>,
}

impl Payload {
    /// The payload in `bytes`, which it keeps.
    pub fn new(bytes: Vec<u8>) -> Self {
        let range = 0..bytes.len();
        Self { segment: Arc::new(Segment::Read(bytes)), range }
    }
}

/// The bytes of a segment, mapped where the filesystem can map it and read into memory where not.
///
/// Reading a segment copies it into a buffer the kernel zeroes a page at a time first, and on a
/// gigabyte of log that zeroing and the copy were a fifth of the CPU an open spent, for bytes the
/// page cache already held. The mapping hands out the cache's own pages. It is safe to hold
/// because nothing writes a segment that replay reads: the lane opens a new segment after the
/// replayed ones, and the replayed ones are only recycled by a checkpoint, after replay has
/// dropped every payload.
enum Segment {
    Read(Vec<u8>),
    Mapped(Mapped),
}

impl Deref for Segment {
    type Target = [u8];

    fn deref(&self) -> &[u8] {
        match self {
            Self::Read(bytes) => bytes,
            Self::Mapped(mapped) => mapped.get(0, mapped.len()).unwrap_or_default(),
        }
    }
}

impl From<Vec<u8>> for Payload {
    fn from(bytes: Vec<u8>) -> Self {
        Self::new(bytes)
    }
}

impl Deref for Payload {
    type Target = [u8];

    fn deref(&self) -> &[u8] {
        &self.segment[self.range.clone()]
    }
}

impl AsRef<[u8]> for Payload {
    fn as_ref(&self) -> &[u8] {
        self
    }
}

impl PartialEq for Payload {
    fn eq(&self, other: &Self) -> bool {
        **self == **other
    }
}

impl Eq for Payload {}

impl std::fmt::Debug for Payload {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_tuple("Payload").field(&&**self).finish()
    }
}

/// A block whose Commit reached the disk, with the records it counts.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Committed {
    /// The segment it was read from.
    pub sequence: u64,
    /// Its Commit.
    pub commit: Commit,
    /// Its records, the Commit left out.
    pub records: Vec<Record>,
}

/// What replaying a lane found.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Replayed {
    /// The committed blocks in the order they were written.
    pub blocks: Vec<Committed>,
    /// Segments read.
    pub segments: u64,
    /// Segments skipped because their header did not verify. A crash while a segment was being
    /// created leaves one, and it never held a record.
    pub unreadable: u64,
}

/// Reads back every committed block of `lane` in `dir`.
///
/// # Errors
///
/// If a segment cannot be read, or a verified header names another lane, sequence or database,
/// which is a file copied in from somewhere else and not something a crash leaves.
pub fn replay(fs: &dyn Filesystem, dir: &Path, lane: u8, database: u64) -> Result<Replayed> {
    let segments = segments(fs, dir, lane)?;
    // A block never spans two segments, so each one is read and verified on its own, and the
    // segments of a large log are read side by side before their blocks are put back in order.
    let threads = std::thread::available_parallelism().map_or(1, usize::from).min(segments.len());
    let read = if threads > 1 {
        let next = AtomicUsize::new(0);
        let mut read: Vec<Option<Result<Option<Vec<Committed>>>>> =
            (0..segments.len()).map(|_| None).collect();
        let done = std::thread::scope(|scope| {
            let workers: Vec<_> = (0..threads)
                .map(|_| {
                    scope.spawn(|| {
                        let mut mine = Vec::new();
                        loop {
                            let at = next.fetch_add(1, Ordering::Relaxed);
                            let Some((sequence, path)) = segments.get(at) else { return mine };
                            mine.push((at, read_segment(fs, path, *sequence, lane, database)));
                        }
                    })
                })
                .collect();
            workers
                .into_iter()
                .flat_map(|worker| worker.join().unwrap_or_default())
                .collect::<Vec<_>>()
        });
        for (at, segment) in done {
            read[at] = Some(segment);
        }
        read.into_iter()
            .map(|segment| segment.unwrap_or_else(|| Err(Error::io("a replay worker stopped"))))
            .collect::<Vec<_>>()
    } else {
        segments
            .iter()
            .map(|(sequence, path)| read_segment(fs, path, *sequence, lane, database))
            .collect()
    };
    let mut replayed = Replayed::default();
    for segment in read {
        match segment? {
            Some(blocks) => {
                replayed.blocks.extend(blocks);
                replayed.segments += 1;
            }
            None => replayed.unreadable += 1,
        }
    }
    Ok(replayed)
}

/// The committed blocks of the segment at `path`, or `None` when its header does not verify.
fn read_segment(
    fs: &dyn Filesystem,
    path: &Path,
    sequence: u64,
    lane: u8,
    database: u64,
) -> Result<Option<Vec<Committed>>> {
    let file = fs.open(path, OpenMode::Read)?;
    let bytes = match file.map() {
        Some(mapped) => Segment::Mapped(mapped),
        None => {
            let len = usize::try_from(file.len()?).map_err(|_| {
                Error::invalid_input(format!("{} is too large to replay", path.display()))
            })?;
            let mut bytes = vec![0_u8; len];
            file.read_exact_at(0, &mut bytes)?;
            Segment::Read(bytes)
        }
    };
    let bytes = Arc::new(bytes);
    let Some(header) = SegmentHeader::decode(&bytes) else {
        return Ok(None);
    };
    if header.lane != lane || header.sequence != sequence || header.database != database {
        return Err(Error::invalid_input(format!(
            "{} holds lane {} segment {} of database {:#x}, not lane {lane} segment {sequence} of {database:#x}",
            path.display(),
            header.lane,
            header.sequence,
            header.database
        )));
    }
    let mut blocks = Vec::new();
    read_blocks(&bytes, SEGMENT_HEADER, lane, sequence, &mut blocks);
    Ok(Some(blocks))
}

/// Appends the committed blocks of the records in `segment` from `at` on to `out`, stopping at the first record
/// that does not verify or a Commit that does not match the records before it.
fn read_blocks(
    segment: &Arc<Segment>,
    mut at: usize,
    lane: u8,
    sequence: u64,
    out: &mut Vec<Committed>,
) {
    let seed = seed(sequence);
    let mut records = Vec::new();
    let mut taken = 0_u64;
    while let Some((header, payload, total)) =
        segment.get(at..).and_then(|rest| decode_record(rest, seed))
    {
        if header.lane != lane {
            break;
        }
        let range = at + RECORD_HEADER..at + RECORD_HEADER + payload.len();
        at += total;
        if header.kind != Kind::Commit {
            taken += total as u64;
            records
                .push(Record { header, payload: Payload { segment: Arc::clone(segment), range } });
            continue;
        }
        let Some(commit) = Commit::decode(payload) else { break };
        let whole = commit.records as usize == records.len()
            && commit.bytes == taken
            && commit.commit_ts == header.gsn;
        if !whole {
            break;
        }
        out.push(Committed { sequence, commit, records: std::mem::take(&mut records) });
        taken = 0;
    }
}
