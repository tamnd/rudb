//! Reading a lane back after a restart, `09-the-log.md` section 9.8.
//!
//! Segments are read in sequence order and each from its first record. The first record that does
//! not verify ends its segment: past it are zeros, a block that was being written when the process
//! stopped, or, in a recycled segment, records of an older sequence that fail under this one's
//! seed. Replay goes on with the next segment, because the lane synced this one before it wrote a
//! byte to that one. Records after a segment's last Commit belong to a block that never finished
//! and are dropped, and nobody was told that block committed.

use std::path::Path;

use rudb_common::{Error, Result};
use rudb_io::{Filesystem, OpenMode};

use super::format::{
    Commit, Kind, RecordHeader, SEGMENT_HEADER, SegmentHeader, decode_record, seed,
};
use super::lane::segments;

/// One record of a committed block.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Record {
    /// Its header.
    pub header: RecordHeader,
    /// Its payload.
    pub payload: Vec<u8>,
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
    let mut replayed = Replayed::default();
    for (sequence, path) in segments(fs, dir, lane)? {
        let file = fs.open(&path, OpenMode::Read)?;
        let len = usize::try_from(file.len()?).map_err(|_| {
            Error::invalid_input(format!("{} is too large to replay", path.display()))
        })?;
        let mut bytes = vec![0_u8; len];
        file.read_exact_at(0, &mut bytes)?;
        let Some(header) = SegmentHeader::decode(&bytes) else {
            replayed.unreadable += 1;
            continue;
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
        read_blocks(&bytes[SEGMENT_HEADER..], lane, sequence, &mut replayed.blocks);
        replayed.segments += 1;
    }
    Ok(replayed)
}

/// Appends the committed blocks of one segment's records to `out`, stopping at the first record
/// that does not verify or a Commit that does not match the records before it.
fn read_blocks(bytes: &[u8], lane: u8, sequence: u64, out: &mut Vec<Committed>) {
    let seed = seed(sequence);
    let mut at = 0;
    let mut records = Vec::new();
    let mut taken = 0_u64;
    while let Some((header, payload, total)) =
        bytes.get(at..).and_then(|rest| decode_record(rest, seed))
    {
        if header.lane != lane {
            break;
        }
        at += total;
        if header.kind != Kind::Commit {
            taken += total as u64;
            records.push(Record { header, payload: payload.to_vec() });
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
