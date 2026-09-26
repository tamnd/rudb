//! One lane of the log: its segments, the blocks committers hand it, and the group commit that
//! gets them to the disk, `09-the-log.md` sections 9.2, 9.5 and 9.6.
//!
//! A committer encodes its block under the lane's lock, because the checksum seed depends on the
//! segment the block lands in and only the lock knows that, and then waits. Whichever waiter finds
//! nobody flushing becomes the leader: it takes every block queued so far, writes each segment's
//! run of them with one `pwritev`, syncs once, and wakes everyone. Blocks that arrive while it is
//! at the disk queue for the next leader, which is what turns a hundred commits into a handful of
//! syncs.
//!
//! A segment is created whole before a record goes into it: the header, zeros to its full size,
//! a sync, and a sync of the directory, so a later write never grows the file and a sync of it
//! never has metadata to carry. A block never spans two segments, and the old segment is synced
//! before the first write to the new one, so replay can treat a bad record as the end of its
//! segment and still read the next one.

use std::mem;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Condvar, Mutex, MutexGuard, PoisonError};

use rudb_common::{Error, Result};
use rudb_io::{File, Filesystem, OpenMode};

use super::format::{
    Commit, Kind, RecordHeader, SEGMENT_BYTES, SEGMENT_HEADER, SegmentHeader, encode_record,
    record_bytes, seed,
};

/// How many zero bytes a new segment is filled with a write at a time.
const ZERO_CHUNK: usize = 1 << 20;

/// What a commit waits for before it returns, the `commit_sync` setting.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum CommitSync {
    /// The block is on stable storage: `F_FULLFSYNC` on macOS and `fdatasync` elsewhere.
    #[default]
    Full,
    /// Meant to be `F_BARRIERFSYNC` on macOS, which orders the writes without flushing the drive's
    /// cache. `rudb-io` has one sync today, so this is [`Self::Full`] until it has two.
    Barrier,
    /// The block is written to the operating system. It survives the process and not the machine.
    Os,
    /// Nothing. The block is queued and goes out with the next commit that waits or the next
    /// [`Lane::flush`].
    None,
}

/// How a lane is set up.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Options {
    /// Which lane this is, which names its segments and is written into every record.
    pub lane: u8,
    /// The database the log belongs to, written into every segment header.
    pub database: u64,
    /// How many bytes a segment is, the header included. 64 MiB unless a test wants less.
    pub segment_bytes: u64,
    /// What a commit waits for.
    pub commit_sync: CommitSync,
}

impl Options {
    /// Lane 0 of `database` with 64 MiB segments and full syncs.
    #[must_use]
    pub fn new(database: u64) -> Self {
        Self { lane: 0, database, segment_bytes: SEGMENT_BYTES, commit_sync: CommitSync::Full }
    }
}

/// The file name of segment `sequence` of `lane`: `L` and the lane in two digits, a dash, and the
/// sequence in sixteen hex digits, so names sort in the order the segments were written.
#[must_use]
pub fn segment_name(lane: u8, sequence: u64) -> String {
    format!("L{lane:02}-{sequence:016x}")
}

/// The lane and sequence a segment's file name says, or `None` if it is not a segment's name.
#[must_use]
pub fn parse_segment_name(name: &str) -> Option<(u8, u64)> {
    let (lane, sequence) = name.strip_prefix('L')?.split_once('-')?;
    if lane.len() != 2 || sequence.len() != 16 {
        return None;
    }
    if !lane.bytes().all(|b| b.is_ascii_digit()) || !sequence.bytes().all(|b| b.is_ascii_hexdigit())
    {
        return None;
    }
    Some((lane.parse().ok()?, u64::from_str_radix(sequence, 16).ok()?))
}

/// The segments of `lane` in `dir`, in the order they were written. No directory is no segments.
///
/// # Errors
///
/// If the directory cannot be listed.
pub fn segments(fs: &dyn Filesystem, dir: &Path, lane: u8) -> Result<Vec<(u64, PathBuf)>> {
    if !fs.is_dir(dir) {
        return Ok(Vec::new());
    }
    let mut found: Vec<(u64, PathBuf)> = fs
        .read_dir(dir)?
        .into_iter()
        .filter_map(|path| {
            let (of, sequence) = parse_segment_name(path.file_name()?.to_str()?)?;
            (of == lane).then_some((sequence, path))
        })
        .collect();
    found.sort_by_key(|&(sequence, _)| sequence);
    Ok(found)
}

/// One transaction's records, which reach the lane together and end with its Commit.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Block {
    txn: u64,
    commit_ts: u64,
    dep_ts: u64,
    /// Each record's kind, flags, and where its payload ends in `data`.
    records: Vec<(Kind, u16, usize)>,
    data: Vec<u8>,
    /// How many bytes the records take in the lane, the Commit left out.
    bytes: usize,
}

impl Block {
    /// An empty block for transaction `txn`, committing at `commit_ts` after `dep_ts`.
    #[must_use]
    pub fn new(txn: u64, commit_ts: u64, dep_ts: u64) -> Self {
        Self { txn, commit_ts, dep_ts, records: Vec::new(), data: Vec::new(), bytes: 0 }
    }

    /// Adds a record. Its `gsn` is the block's commit timestamp.
    ///
    /// # Errors
    ///
    /// If `kind` is [`Kind::Commit`], which the lane writes itself, or the payload is 4 GiB or
    /// longer.
    pub fn push(&mut self, kind: Kind, flags: u16, payload: &[u8]) -> Result<()> {
        if kind == Kind::Commit {
            return Err(Error::invalid_input("a block's Commit record is written by the lane"));
        }
        if u32::try_from(payload.len()).is_err() {
            return Err(Error::invalid_input("a log record's payload is under 4 GiB"));
        }
        self.data.extend_from_slice(payload);
        self.records.push((kind, flags, self.data.len()));
        self.bytes += record_bytes(payload.len());
        Ok(())
    }

    /// How many records it has, the Commit left out.
    #[must_use]
    pub fn records(&self) -> usize {
        self.records.len()
    }

    /// How many bytes it takes in the lane, the Commit included.
    #[must_use]
    pub fn len(&self) -> usize {
        self.bytes + record_bytes(Commit::LEN)
    }

    /// Whether it has no records apart from its Commit.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.records.is_empty()
    }

    fn encode(&self, lane: u8, seed: u32, out: &mut Vec<u8>) {
        let mut start = 0;
        for &(kind, flags, end) in &self.records {
            let header = RecordHeader { kind, lane, flags, gsn: self.commit_ts };
            encode_record(out, &header, &self.data[start..end], seed);
            start = end;
        }
        let commit = Commit {
            txn: self.txn,
            commit_ts: self.commit_ts,
            dep_ts: self.dep_ts,
            records: self.records.len() as u32,
            bytes: self.bytes as u64,
        };
        let header = RecordHeader { kind: Kind::Commit, lane, flags: 0, gsn: self.commit_ts };
        encode_record(out, &header, &commit.encode(), seed);
    }
}

/// What a lane has done since it was opened.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Stats {
    /// Bytes of blocks committed, headers and padding included.
    pub bytes: u64,
    /// Blocks committed.
    pub commits: u64,
    /// `pwritev` calls that carried them.
    pub writes: u64,
    /// Syncs of a segment, the ones that created it left out.
    pub syncs: u64,
    /// Segments created.
    pub segments: u64,
}

/// An encoded block waiting for a leader to write it.
#[derive(Debug)]
struct Piece {
    sequence: u64,
    offset: u64,
    /// The lane position just past it.
    end: u64,
    bytes: Vec<u8>,
}

/// The segment writes go to.
#[derive(Debug)]
struct Open {
    sequence: u64,
    file: Box<dyn File>,
    /// Written to since its last sync.
    dirty: bool,
}

#[derive(Debug)]
struct State {
    /// Where the next block goes.
    tail_sequence: u64,
    tail_offset: u64,
    pending: Vec<Piece>,
    /// The segment being written, which the leader holds while it flushes.
    open: Option<Open>,
    /// Lane positions, counted in bytes of blocks from the lane's opening: every block reserved,
    /// every block written, and every block synced.
    reserved: u64,
    written: u64,
    durable: u64,
    flushing: bool,
    /// Why the lane stopped. A write or sync that fails leaves the disk in a state nobody can
    /// say, so the lane refuses everything after it and the database goes read only.
    failed: Option<String>,
    stats: Stats,
}

impl State {
    fn check(&self) -> Result<()> {
        match &self.failed {
            Some(why) => {
                Err(Error::io(format!("the log failed earlier and takes no more commits: {why}")))
            }
            None => Ok(()),
        }
    }
}

/// What one leader's turn at the disk did.
#[derive(Debug, Default)]
struct Led {
    written: u64,
    durable: Option<u64>,
    writes: u64,
    syncs: u64,
    segments: u64,
}

/// One lane of the log, shared by every committer.
#[derive(Debug)]
pub struct Lane {
    fs: Arc<dyn Filesystem>,
    dir: PathBuf,
    options: Options,
    state: Mutex<State>,
    changed: Condvar,
}

impl Lane {
    /// Opens the lane for writing in `dir`, creating the directory if it is not there.
    ///
    /// Writing always starts in a new segment, one past the last one there. A crash can leave
    /// blocks nobody was told were committed after the last good record of a segment, and a
    /// fresh segment keeps new records from ever being read after them. Replay the lane first:
    /// this does not read what is there.
    ///
    /// # Errors
    ///
    /// If the segment size is under two headers or not a multiple of 8, or the directory or the
    /// first segment cannot be created.
    pub fn open(fs: Arc<dyn Filesystem>, dir: &Path, options: Options) -> Result<Self> {
        if options.segment_bytes < 2 * SEGMENT_HEADER as u64
            || !options.segment_bytes.is_multiple_of(8)
        {
            return Err(Error::invalid_input(format!(
                "a log segment of {} bytes is under two headers or not a multiple of 8",
                options.segment_bytes
            )));
        }
        if !fs.is_dir(dir) {
            fs.create_dir_all(dir)?;
            if let Some(parent) = dir.parent().filter(|parent| !parent.as_os_str().is_empty()) {
                fs.sync_dir(parent)?;
            }
        }
        let sequence =
            segments(fs.as_ref(), dir, options.lane)?.last().map_or(1, |(last, _)| last + 1);
        let lane = Self {
            fs,
            dir: dir.to_path_buf(),
            options,
            state: Mutex::new(State {
                tail_sequence: sequence,
                tail_offset: SEGMENT_HEADER as u64,
                pending: Vec::new(),
                open: None,
                reserved: 0,
                written: 0,
                durable: 0,
                flushing: false,
                failed: None,
                stats: Stats::default(),
            }),
            changed: Condvar::new(),
        };
        let file = lane.create_segment(sequence)?;
        let mut state = lane.lock();
        state.open = Some(Open { sequence, file, dirty: false });
        state.stats.segments = 1;
        drop(state);
        Ok(lane)
    }

    /// Commits `block` and returns its lane position once `commit_sync` says it may.
    ///
    /// # Errors
    ///
    /// If the block is larger than a segment, which needs the spill that is not written yet, or
    /// the lane failed, now or earlier.
    pub fn commit(&self, block: &Block) -> Result<u64> {
        let len = block.len() as u64;
        if len > self.options.segment_bytes - SEGMENT_HEADER as u64 {
            return Err(Error::invalid_input(format!(
                "a log block of {len} bytes does not fit a segment of {} bytes",
                self.options.segment_bytes
            )));
        }
        let mut state = self.lock();
        state.check()?;
        if state.tail_offset + len > self.options.segment_bytes {
            state.tail_sequence += 1;
            state.tail_offset = SEGMENT_HEADER as u64;
        }
        let mut bytes = Vec::with_capacity(block.len());
        block.encode(self.options.lane, seed(state.tail_sequence), &mut bytes);
        let end = state.reserved + len;
        let piece = Piece { sequence: state.tail_sequence, offset: state.tail_offset, end, bytes };
        state.pending.push(piece);
        state.tail_offset += len;
        state.reserved = end;
        state.stats.bytes += len;
        state.stats.commits += 1;
        match self.options.commit_sync {
            CommitSync::Full | CommitSync::Barrier => self.wait(state, end, true),
            CommitSync::Os => self.wait(state, end, false),
            CommitSync::None => Ok(end),
        }
    }

    /// Writes and syncs every block committed so far.
    ///
    /// # Errors
    ///
    /// If the lane failed, now or earlier.
    pub fn flush(&self) -> Result<()> {
        let state = self.lock();
        let end = state.reserved;
        self.wait(state, end, true).map(|_| ())
    }

    /// The lane position every block before which is on stable storage.
    #[must_use]
    pub fn durable(&self) -> u64 {
        self.lock().durable
    }

    /// Where the next block goes: the sequence of its segment and its offset there, which is where
    /// a replay that wants nothing written so far would start.
    #[must_use]
    pub fn position(&self) -> (u64, u64) {
        let state = self.lock();
        (state.tail_sequence, state.tail_offset)
    }

    /// What the lane has done since it was opened.
    #[must_use]
    pub fn stats(&self) -> Stats {
        self.lock().stats
    }

    /// The directory the lane's segments are in.
    #[must_use]
    pub fn dir(&self) -> &Path {
        &self.dir
    }

    fn lock(&self) -> MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Waits until lane position `end` is written, or synced when `durable`, leading a flush
    /// whenever nobody else is.
    fn wait<'a>(
        &'a self,
        mut state: MutexGuard<'a, State>,
        end: u64,
        durable: bool,
    ) -> Result<u64> {
        loop {
            state.check()?;
            let reached = if durable { state.durable } else { state.written };
            if reached >= end {
                return Ok(end);
            }
            if state.flushing {
                state = self.changed.wait(state).unwrap_or_else(PoisonError::into_inner);
                continue;
            }
            let Some(mut open) = state.open.take() else {
                return Err(Error::internal(
                    "the log lane has no open segment and nobody flushing",
                ));
            };
            state.flushing = true;
            let pieces = mem::take(&mut state.pending);
            let written = state.written;
            drop(state);
            let outcome = self.lead(&mut open, &pieces, written, durable);
            drop(pieces);
            state = self.lock();
            state.flushing = false;
            state.open = Some(open);
            match outcome {
                Ok(led) => {
                    state.written = state.written.max(led.written);
                    if let Some(synced) = led.durable {
                        state.durable = state.durable.max(synced);
                    }
                    state.stats.writes += led.writes;
                    state.stats.syncs += led.syncs;
                    state.stats.segments += led.segments;
                }
                Err(error) => state.failed = Some(error.to_string()),
            }
            self.changed.notify_all();
        }
    }

    /// Writes `pieces`, which start at lane position `written`, a segment's run at a time, and
    /// syncs at the end when `sync`.
    fn lead(&self, open: &mut Open, pieces: &[Piece], mut written: u64, sync: bool) -> Result<Led> {
        let mut led = Led::default();
        let mut start = 0;
        while start < pieces.len() {
            let sequence = pieces[start].sequence;
            let stop = pieces[start..]
                .iter()
                .position(|piece| piece.sequence != sequence)
                .map_or(pieces.len(), |run| start + run);
            if sequence != open.sequence {
                // The old segment is synced before the new one gets a byte, so a segment with
                // records in it always follows one that is complete on the disk.
                if open.dirty {
                    open.file.sync()?;
                    led.syncs += 1;
                }
                led.durable = Some(written);
                *open = Open { sequence, file: self.create_segment(sequence)?, dirty: false };
                led.segments += 1;
            }
            let parts: Vec<&[u8]> =
                pieces[start..stop].iter().map(|piece| piece.bytes.as_slice()).collect();
            open.file.write_parts_at(pieces[start].offset, &parts)?;
            open.dirty = true;
            led.writes += 1;
            written = pieces[stop - 1].end;
            start = stop;
        }
        if sync {
            if open.dirty {
                open.file.sync()?;
                open.dirty = false;
                led.syncs += 1;
            }
            led.durable = Some(written);
        }
        led.written = written;
        Ok(led)
    }

    /// Creates segment `sequence` whole: header, zeros to its size, a sync, and a sync of the
    /// directory so the name survives too.
    fn create_segment(&self, sequence: u64) -> Result<Box<dyn File>> {
        let path = self.dir.join(segment_name(self.options.lane, sequence));
        let file = self.fs.open(&path, OpenMode::CreateNew)?;
        let header =
            SegmentHeader { lane: self.options.lane, sequence, database: self.options.database };
        file.write_at(0, &header.encode())?;
        let size = self.options.segment_bytes;
        let zeros = vec![0_u8; ZERO_CHUNK.min((size - SEGMENT_HEADER as u64) as usize)];
        let mut at = SEGMENT_HEADER as u64;
        while at < size {
            let run = zeros.len().min((size - at) as usize);
            file.write_at(at, &zeros[..run])?;
            at += run as u64;
        }
        file.sync()?;
        self.fs.sync_dir(&self.dir)?;
        Ok(file)
    }
}
