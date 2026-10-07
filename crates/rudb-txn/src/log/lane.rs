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
//! never has metadata to carry, which is why the lane syncs with `fdatasync`. A block never spans two segments, and the old segment is synced
//! before the first write to the new one, so replay can treat a bad record as the end of its
//! segment and still read the next one.
//!
//! A committer whose blocks went to a leader already at the disk waits for that leader the way
//! `13-the-point-path.md` section 13.7 says: when the lane's recent turns at the disk took under
//! [`SPIN_UNDER`], it spins on the count of turns, because parking and waking a thread would be a
//! large share of a commit that short, and otherwise it parks on the lane's condition variable and
//! the leader wakes everyone at once. A spin that runs out parks all the same.
//!
//! Filling a segment with zeros and syncing it is the slow part, and a commit that starts one would
//! otherwise do it while every committer behind it waits. So the lane keeps a couple of spares, the
//! segments a checkpoint retired or ones a thread of its own filled with zeros ahead of time, and a
//! new segment is a spare with a new header and a new name.

use std::mem;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex, MutexGuard, PoisonError};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use rudb_common::{Error, Result};
use rudb_io::{File, Filesystem, OpenMode};

use super::format::{
    Commit, Kind, RecordHeader, SEGMENT_BYTES, SEGMENT_HEADER, SegmentHeader, encode_record,
    record_bytes, seed,
};

/// How many zero bytes a new segment is filled with a write at a time.
const ZERO_CHUNK: usize = 1 << 20;

/// How many retired segments a lane keeps to recycle, `09-the-log.md` section 9.3. A checkpoint
/// retires the segments behind it, so two cover a log that checkpoints every couple of segments.
const SPARES: usize = 2;

/// The first sequence a spare the lane made itself is named under. A segment's sequence counts up
/// from 1 and never gets near it, so such a spare's name is never one a retired segment takes.
const MADE: u64 = 1 << 63;

/// A leader's turn at the disk shorter than this, on the lane's recent average, is one a waiter
/// spins through rather than parks for, `13-the-point-path.md` section 13.7.
const SPIN_UNDER: Duration = Duration::from_micros(50);

/// What a commit waits for before it returns, the `commit_sync` setting.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum CommitSync {
    /// The block is on stable storage: `F_FULLFSYNC` on macOS and `fdatasync` elsewhere.
    #[default]
    Full,
    /// The block is ordered on the device: `F_BARRIERFSYNC` on macOS, which keeps every write
    /// before it ahead of every write after it without waiting for the drive to empty its cache.
    /// A power loss can lose the last commits and never one commit while keeping a later one.
    /// Elsewhere there is no such call and this is [`Self::Full`].
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
    /// Whether the lane fills spares with zeros on a thread of its own whenever it has fewer than
    /// `SPARES`, so a commit that starts a segment renames one rather than writes the whole
    /// segment while every committer behind it waits. Off for a test that counts the operations
    /// the lane does, which a thread of its own would make vary from run to run.
    pub spare_ahead: bool,
}

impl Options {
    /// Lane 0 of `database` with 64 MiB segments and full syncs.
    #[must_use]
    pub fn new(database: u64) -> Self {
        Self {
            lane: 0,
            database,
            segment_bytes: SEGMENT_BYTES,
            commit_sync: CommitSync::Full,
            spare_ahead: true,
        }
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

/// The name a retired segment of `lane` waits under until the lane recycles it: its old name with
/// `.spare` after it, which [`parse_segment_name`] does not read as a segment's, so neither replay
/// nor [`segments`] sees it.
fn spare_name(lane: u8, sequence: u64) -> String {
    format!("{}.spare", segment_name(lane, sequence))
}

/// The retired segments of `lane` in `dir` waiting to be recycled.
///
/// # Errors
///
/// If the directory cannot be listed.
pub fn spares(fs: &dyn Filesystem, dir: &Path, lane: u8) -> Result<Vec<PathBuf>> {
    if !fs.is_dir(dir) {
        return Ok(Vec::new());
    }
    Ok(fs
        .read_dir(dir)?
        .into_iter()
        .filter(|path| {
            path.file_name()
                .and_then(|name| name.to_str()?.strip_suffix(".spare"))
                .and_then(parse_segment_name)
                .is_some_and(|(of, _)| of == lane)
        })
        .collect())
}

/// Removes a spare. A spare that is gone already is no error, because its removal is all that was
/// wanted. A second lane on the same directory, such as the lane of a database that a test drops
/// without closing, can recycle a spare or remove it between the listing and the removal.
///
/// # Errors
///
/// If the spare is still there after the removal failed.
pub fn remove_spare(fs: &dyn Filesystem, path: &Path) -> Result<()> {
    match fs.remove(path) {
        Err(error) if fs.exists(path) => Err(error),
        _ => Ok(()),
    }
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
    /// Of those, the ones made from a spare rather than filled with zeros on the spot.
    pub recycled: u64,
    /// Spares the lane filled with zeros on a thread of its own.
    pub made: u64,
    /// Waits for another committer's turn at the disk that spun until it ended.
    pub spun: u64,
    /// Waits that parked until it did.
    pub parked: u64,
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
    /// Every block ordered behind a barrier, or synced, which is more.
    ordered: u64,
    flushing: bool,
    /// Why the lane stopped. A write or sync that fails leaves the disk in a state nobody can
    /// say, so the lane refuses everything after it and the database goes read only.
    failed: Option<String>,
    stats: Stats,
    /// Retired segments and spares made ahead, full size, that the next new segments are made
    /// from.
    spares: Vec<PathBuf>,
    /// The sequence the next spare made ahead is named under, from [`MADE`] up.
    made: u64,
    /// A thread is making spares.
    making: bool,
    /// The lane is closing and starts no more of them.
    stopped: bool,
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

/// How far a waiter needs its blocks to have got.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Reach {
    /// Handed to the operating system.
    Written,
    /// Ordered on the device behind a barrier.
    Ordered,
    /// On stable storage.
    Durable,
}

/// What one leader's turn at the disk did.
#[derive(Debug, Default)]
struct Led {
    written: u64,
    durable: Option<u64>,
    ordered: Option<u64>,
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
    state: Arc<Mutex<State>>,
    changed: Condvar,
    /// How many turns leaders have finished at the disk, raised under the lock before the wake,
    /// which is what a spinning waiter watches.
    turns: AtomicU64,
    /// The recent average of a turn in nanoseconds, which says whether to spin.
    turn_ns: AtomicU64,
    /// The thread making spares ahead, joined before another starts and when the lane stops.
    maker: Mutex<Option<JoinHandle<()>>>,
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
        // Spares a run before this one retired are recycled like this run's own, unless the
        // segment size changed since.
        let mut kept = Vec::new();
        let mut made = MADE;
        for spare in spares(fs.as_ref(), dir, options.lane)? {
            if kept.len() < SPARES
                && fs.open(&spare, OpenMode::Read)?.len()? == options.segment_bytes
            {
                if let Some((_, sequence)) = spare_sequence(&spare) {
                    made = made.max(sequence.saturating_add(1));
                }
                kept.push(spare);
            } else {
                remove_spare(fs.as_ref(), &spare)?;
            }
        }
        let lane = Self {
            fs,
            dir: dir.to_path_buf(),
            options,
            state: Arc::new(Mutex::new(State {
                tail_sequence: sequence,
                tail_offset: SEGMENT_HEADER as u64,
                pending: Vec::new(),
                open: None,
                reserved: 0,
                written: 0,
                durable: 0,
                ordered: 0,
                flushing: false,
                failed: None,
                stats: Stats::default(),
                spares: kept,
                made,
                making: false,
                stopped: false,
            })),
            changed: Condvar::new(),
            turns: AtomicU64::new(0),
            turn_ns: AtomicU64::new(0),
            maker: Mutex::new(None),
        };
        let file = lane.create_segment(sequence)?;
        let mut state = lane.lock();
        state.open = Some(Open { sequence, file, dirty: false });
        state.stats.segments = 1;
        drop(state);
        lane.spare_ahead();
        Ok(lane)
    }

    /// Commits `block` and returns its lane position once `commit_sync` says it may.
    ///
    /// # Errors
    ///
    /// If the block is larger than a segment, which needs the spill that is not written yet, or
    /// the lane failed, now or earlier.
    pub fn commit(&self, block: &Block) -> Result<u64> {
        let end = self.enqueue(block)?;
        self.settle(end, self.options.commit_sync)
    }

    /// Queues `block` for the next leader and returns the lane position just past it, without
    /// waiting for anything. Until [`Self::settle`] says so it may be neither written nor synced.
    ///
    /// A committer that queues under its own lock and waits after letting go of it is what lets a
    /// second committer's block into the same sync.
    ///
    /// # Errors
    ///
    /// If the block is larger than a segment, or the lane failed earlier.
    pub fn enqueue(&self, block: &Block) -> Result<u64> {
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
        Ok(end)
    }

    /// Waits until lane position `end`, from [`Self::enqueue`], is as far as `sync` asks: synced
    /// for `full` and `barrier`, handed to the operating system for `os`, and nowhere for `none`.
    ///
    /// # Errors
    ///
    /// If the lane failed, now or earlier.
    pub fn settle(&self, end: u64, sync: CommitSync) -> Result<u64> {
        match sync {
            CommitSync::Full => self.wait(self.lock(), end, Reach::Durable),
            CommitSync::Barrier => self.wait(self.lock(), end, Reach::Ordered),
            CommitSync::Os => self.wait(self.lock(), end, Reach::Written),
            CommitSync::None => Ok(end),
        }
    }

    /// Changes what the next commit waits for.
    pub fn set_commit_sync(&mut self, sync: CommitSync) {
        self.options.commit_sync = sync;
    }

    /// How many bytes of committed blocks have not been handed to the operating system yet, which
    /// under [`CommitSync::None`] is what a crash of the process would lose.
    #[must_use]
    pub fn unwritten(&self) -> u64 {
        let state = self.lock();
        state.reserved - state.written.min(state.reserved)
    }

    /// Writes every block committed so far to the operating system, without a sync.
    ///
    /// # Errors
    ///
    /// If the lane failed, now or earlier.
    pub fn write_out(&self) -> Result<()> {
        let state = self.lock();
        let end = state.reserved;
        self.wait(state, end, Reach::Written).map(|_| ())
    }

    /// Writes and syncs every block committed so far.
    ///
    /// # Errors
    ///
    /// If the lane failed, now or earlier.
    pub fn flush(&self) -> Result<()> {
        let state = self.lock();
        let end = state.reserved;
        self.wait(state, end, Reach::Durable).map(|_| ())
    }

    /// Whether a block committed so far is not yet on stable storage.
    #[must_use]
    pub fn behind(&self) -> bool {
        let state = self.lock();
        state.reserved > state.durable
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

    /// Retires every segment before `below` that is not being written, once a checkpoint has made
    /// them redundant: up to `SPARES` of them are kept under a spare name to be recycled as the
    /// lane's next segments, and the rest are removed.
    ///
    /// The directory is synced before this returns, so a spare's header is never rewritten while
    /// a crash could still bring back its old name.
    ///
    /// # Errors
    ///
    /// If a segment cannot be renamed or removed, or the directory cannot be synced.
    pub fn retire(&self, below: u64) -> Result<()> {
        let (writing, mut wanted) = {
            let state = self.lock();
            (
                state.open.as_ref().map(|open| open.sequence),
                SPARES.saturating_sub(state.spares.len()),
            )
        };
        let mut retired = Vec::new();
        let mut changed = false;
        for (sequence, path) in segments(self.fs.as_ref(), &self.dir, self.options.lane)? {
            if sequence >= below || Some(sequence) == writing {
                continue;
            }
            changed = true;
            if wanted > 0
                && self.fs.open(&path, OpenMode::Read)?.len()? == self.options.segment_bytes
            {
                let spare = self.dir.join(spare_name(self.options.lane, sequence));
                self.fs.rename(&path, &spare)?;
                retired.push(spare);
                wanted -= 1;
            } else {
                self.fs.remove(&path)?;
            }
        }
        if changed {
            self.fs.sync_dir(&self.dir)?;
        }
        self.lock().spares.extend(retired);
        Ok(())
    }

    /// Stops making spares ahead and waits for a spare being made to be done, so nothing creates
    /// a file in the directory after this returns. For a lane whose log is about to be removed.
    pub fn stop(&self) {
        self.lock().stopped = true;
        self.join_maker();
    }

    /// Starts a thread making spares when the lane has fewer than [`SPARES`] and none is at it.
    fn spare_ahead(&self) {
        if !self.options.spare_ahead {
            return;
        }
        {
            let mut state = self.lock();
            if state.making
                || state.stopped
                || state.failed.is_some()
                || state.spares.len() >= SPARES
            {
                return;
            }
            state.making = true;
        }
        self.join_maker();
        let (fs, dir, state) = (Arc::clone(&self.fs), self.dir.clone(), Arc::clone(&self.state));
        let (lane, size) = (self.options.lane, self.options.segment_bytes);
        let spawned = std::thread::Builder::new()
            .name("rudb-log-spares".into())
            .spawn(move || make_spares(fs.as_ref(), &dir, lane, size, &state));
        match spawned {
            Ok(handle) => {
                *self.maker.lock().unwrap_or_else(PoisonError::into_inner) = Some(handle);
            }
            // No thread means the next segment is filled on the spot, as it was before.
            Err(_) => self.lock().making = false,
        }
    }

    fn join_maker(&self) {
        let handle = self.maker.lock().unwrap_or_else(PoisonError::into_inner).take();
        if let Some(handle) = handle {
            let _ = handle.join();
        }
    }

    /// The directory the lane's segments are in.
    #[must_use]
    pub fn dir(&self) -> &Path {
        &self.dir
    }

    fn lock(&self) -> MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Waits until lane position `end` is as far as `reach`, leading a flush whenever nobody else
    /// is.
    fn wait<'a>(&'a self, mut state: MutexGuard<'a, State>, end: u64, reach: Reach) -> Result<u64> {
        loop {
            state.check()?;
            let reached = match reach {
                Reach::Written => state.written,
                Reach::Ordered => state.ordered.max(state.durable),
                Reach::Durable => state.durable,
            };
            if reached >= end {
                return Ok(end);
            }
            if state.flushing {
                state = self.await_turn(state);
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
            let started = Instant::now();
            let outcome = self.lead(&mut open, &pieces, written, reach);
            let took = u64::try_from(started.elapsed().as_nanos()).unwrap_or(u64::MAX);
            let average = self.turn_ns.load(Ordering::Relaxed);
            self.turn_ns.store(average - average / 4 + took / 4, Ordering::Relaxed);
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
                    if let Some(ordered) = led.ordered {
                        state.ordered = state.ordered.max(ordered);
                    }
                    state.stats.writes += led.writes;
                    state.stats.syncs += led.syncs;
                    state.stats.segments += led.segments;
                    if led.segments > 0 && state.spares.len() < SPARES && !state.making {
                        drop(state);
                        self.spare_ahead();
                        state = self.lock();
                    }
                }
                Err(error) => state.failed = Some(error.to_string()),
            }
            self.turns.fetch_add(1, Ordering::Release);
            self.changed.notify_all();
        }
    }

    /// Waits for the leader at the disk to finish its turn, spinning when turns are short and
    /// parking otherwise, and hands the lock back for the caller to look again.
    ///
    /// A leader raises `turns` holding the lock and wakes the parked after, so a waiter that looks
    /// under the lock and finds the count unchanged parks before the wake and cannot miss it.
    fn await_turn<'a>(&'a self, state: MutexGuard<'a, State>) -> MutexGuard<'a, State> {
        let seen = self.turns.load(Ordering::Acquire);
        let mut state = state;
        let average = Duration::from_nanos(self.turn_ns.load(Ordering::Relaxed));
        if average < SPIN_UNDER {
            drop(state);
            let started = Instant::now();
            let mut spins = 0_u32;
            while self.turns.load(Ordering::Acquire) == seen {
                spins = spins.wrapping_add(1);
                // The clock is read every so often, a spin being far shorter than a read of it.
                if spins.is_multiple_of(64) && started.elapsed() >= SPIN_UNDER {
                    break;
                }
                std::hint::spin_loop();
            }
            state = self.lock();
            if self.turns.load(Ordering::Acquire) != seen || !state.flushing {
                state.stats.spun += 1;
                return state;
            }
        }
        state.stats.parked += 1;
        self.changed.wait(state).unwrap_or_else(PoisonError::into_inner)
    }

    /// Writes `pieces`, which start at lane position `written`, a segment's run at a time, and
    /// syncs or puts a barrier behind them at the end when `reach` asks for it.
    fn lead(
        &self,
        open: &mut Open,
        pieces: &[Piece],
        mut written: u64,
        reach: Reach,
    ) -> Result<Led> {
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
                    open.file.sync_data()?;
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
        match reach {
            Reach::Written => {}
            // A barrier leaves the segment dirty, because what is behind it is ordered and not yet
            // durable, and the next sync still has to be made.
            Reach::Ordered => {
                if open.dirty {
                    open.file.sync_barrier()?;
                    led.syncs += 1;
                }
                led.ordered = Some(written);
            }
            Reach::Durable => {
                if open.dirty {
                    open.file.sync_data()?;
                    open.dirty = false;
                    led.syncs += 1;
                }
                led.durable = Some(written);
            }
        }
        led.written = written;
        Ok(led)
    }

    /// Creates segment `sequence` whole: header, zeros to its size, a sync, and a sync of the
    /// directory so the name survives too.
    ///
    /// A spare is recycled instead when there is one: its header is rewritten and synced under the
    /// spare name and only then renamed, so a crash leaves either a spare, which nothing reads, or
    /// a segment whose header says its name. The records it still holds were seeded with its old
    /// sequence and fail under the new one, so replay stops where the new records do.
    fn create_segment(&self, sequence: u64) -> Result<Box<dyn File>> {
        let path = self.dir.join(segment_name(self.options.lane, sequence));
        let header =
            SegmentHeader { lane: self.options.lane, sequence, database: self.options.database };
        let spare = self.lock().spares.pop();
        if let Some(spare) = spare {
            let file = self.fs.open(&spare, OpenMode::ReadWrite)?;
            file.write_at(0, &header.encode())?;
            file.sync_data()?;
            self.fs.rename(&spare, &path)?;
            self.fs.sync_dir(&self.dir)?;
            self.lock().stats.recycled += 1;
            return Ok(file);
        }
        let file = self.fs.open(&path, OpenMode::CreateNew)?;
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

impl Drop for Lane {
    fn drop(&mut self) {
        self.stop();
    }
}

/// The lane and sequence a spare's file name says.
fn spare_sequence(path: &Path) -> Option<(u8, u64)> {
    parse_segment_name(path.file_name()?.to_str()?.strip_suffix(".spare")?)
}

/// Makes spares of `size` bytes for `lane` in `dir` until the lane has [`SPARES`] of them or
/// stops, each filled with zeros and synced, with the directory, before the lane is given it.
///
/// A spare is all zeros, header included, which is what a retired segment's records come to as
/// far as a new sequence is concerned: [`Lane::create_segment`] writes the header when it renames
/// the spare, and nothing after it reads as a record. One that fails halfway is removed, and the
/// lane fills its next segment on the spot as it would have without a thread.
fn make_spares(fs: &dyn Filesystem, dir: &Path, lane: u8, size: u64, state: &Mutex<State>) {
    let lock = || state.lock().unwrap_or_else(PoisonError::into_inner);
    loop {
        let path = {
            let mut state = lock();
            if state.stopped || state.failed.is_some() || state.spares.len() >= SPARES {
                state.making = false;
                return;
            }
            state.made += 1;
            dir.join(spare_name(lane, state.made - 1))
        };
        // A file that is there already under the name is not this thread's to remove. Only a
        // second lane on the same directory makes one.
        let Ok(file) = fs.open(&path, OpenMode::CreateNew) else {
            lock().making = false;
            return;
        };
        if fill(file.as_ref(), size).and_then(|()| fs.sync_dir(dir)).is_err() {
            let _ = fs.remove(&path);
            lock().making = false;
            return;
        }
        let mut state = lock();
        state.spares.insert(0, path);
        state.stats.made += 1;
    }
}

/// Writes `size` zero bytes to a new `file` and syncs it.
fn fill(file: &dyn File, size: u64) -> Result<()> {
    let zeros = vec![0_u8; ZERO_CHUNK.min(size as usize)];
    let mut at = 0;
    while at < size {
        let run = zeros.len().min((size - at) as usize);
        file.write_at(at, &zeros[..run])?;
        at += run as u64;
    }
    file.sync()
}
