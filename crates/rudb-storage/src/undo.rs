//! Undo records, `engine-v4/07-the-head.md` section 7.5.
//!
//! An in-place update of a hot row moves the values it overwrites into an undo record, and the
//! row's `undo` column points at its newest record. A reader whose snapshot does not see a change
//! walks the chain from there and puts the old values back over its copy of the row.
//!
//! Records live in 64 KiB chunks of one [`UndoSpace`] per database, up to 2^20 of them, and each
//! worker bump-allocates its transactions' records in its own [`UndoBuffer`]. An [`UndoRef`] is a
//! u32: 20 bits of chunk and 12 bits of offset in 16-byte units. Chunk ids start at 1, so 0 is the
//! reference of no record.
//!
//! A record is written once before it is published by a release store of its reference, and its
//! `ts` is the only thing written after, with its own atomic store, apart from its `prev`, which
//! the collector moves past a record it takes out of the chain. The words are relaxed atomics
//! for the same reason the hot stripe's cells are: a reader that holds a reference it read in a
//! race still reads defined values.
//!
//! A chunk a worker filled is sealed, and [`UndoSpace::collect`] gives it back once nobody can
//! read it, `08-concurrency.md` section 8.9. That takes three rounds. The first finds every record
//! in the chunk committed at or below the horizon of its table, or aborted. The second, once every
//! statement that was running at the first has ended, takes the records out of their chains. The
//! wait is for an aborted record: a reader that copied a value before the abort put the old one
//! back needs the record to undo it, and that reader began before the abort. The third, once every
//! statement running at the second has ended, puts the chunk on the free list, because a reader
//! that loaded a reference just before it was cut may still read the record's `ts`.
//!
//! A record above the horizon can be needed by nobody too, the interval pruning of section 8.9 after
//! Alhomssi and Leis. An update committed at `T` over one committed at `P` is read only by a
//! snapshot from `P` up to `T`: one before `P` reads the older record's values after it, and one
//! at `T` or later stops at it. When no statement holds a snapshot in between and the older record
//! has every column this one has, the collector takes it out of its chain on its own, and from
//! then on it counts as below the horizon. Its chunk still waits for the statements that were
//! running when it was taken out, since one of them may hold a reference to it. A long transaction
//! then keeps the versions it reads, not every version since it began, once the statement it was
//! running has ended.
//!
//! What an old snapshot does need can still outgrow memory, and past [`SpillLimits::at`] the
//! collector writes it to a file in the temp directory, `15-resources.md` section 15.3. It takes
//! the oldest chunks still waiting, and for each row with a record in them the row's newest
//! committed record and everything older in its chain. Those go to the end of the file, and a stub
//! in the chain stands for them: a record of its own kind, stamped with the newest one's
//! timestamp, whose one image says where they are. A reader at that timestamp or later stops at the stub as it
//! would have stopped at the record. One before it reads them back, and only an old snapshot is
//! before it. The records in memory are then out of their chains, as pruned ones are. Once every
//! stub is gone the file starts again from nothing. A record that would take the file past
//! [`SpillLimits::most`] is not written, and the oldest snapshot is killed instead, see
//! [`Horizons::kill_oldest`].
//!
//! Pruning leaves holes. A chunk whose records are all out of their chains but a few still waits
//! for those few, and one old snapshot that reads a record in each of a hundred chunks keeps 6.4
//! MiB for a few KiB. So once the records left in a chunk take an eighth of it or less, the
//! collector copies them into a chunk of its own and puts each copy in its chain where the record
//! was, and the chunk counts as pruned from then on. A reader on its way down the chain from the
//! old record reads the same records as one from the copy. Over the spill limit, spilling comes
//! first: a spilled record leaves a stub of a few words where a copy is the whole record.
//!
//! The collector runs every millisecond on whichever worker ends a statement first after the
//! deadline, section 8.9, through [`UndoSpace::collect_due`], so nobody waits for a thread of its
//! own to wake up.

use std::collections::{HashMap, HashSet};
use std::fs::File;
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::PathBuf;
use std::sync::atomic::{AtomicU32, AtomicU64, Ordering};
use std::sync::{Mutex, OnceLock, PoisonError};

use crate::deletes::UNCOMMITTED;
use crate::horizon::{Horizons, Killed};

/// Words in a chunk: 64 KiB.
const CHUNK_WORDS: usize = 8_192;

/// Bytes in a chunk.
const CHUNK_BYTES: u64 = CHUNK_WORDS as u64 * 8;

/// Chunks in a directory, and directories in a space, for 2^20 chunks.
const DIRECTORY: usize = 1_024;

/// Words of a record's header: prev, kind and image count; ts; rid; table.
const HEADER_WORDS: usize = 4;

/// Words of one column image: the column and its validity, then the 16-byte value.
const IMAGE_WORDS: usize = 3;

/// The most images one record holds. A wider update writes several records.
pub const MOST_IMAGES: usize = (CHUNK_WORDS - HEADER_WORDS) / IMAGE_WORDS;

/// The reference of an undo record, or of none when it is 0.
pub type UndoRef = u32;

/// The stamp of an undo record whose writer aborted. It is nobody's id, not even that of a reader
/// with none, and like an id it is past every snapshot: every reader applies the record, which
/// holds the values the abort put back, and a writer checking for a newer committed change skips
/// it.
pub(crate) const ABORTED: u64 = u64::MAX;

/// What a record undoes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UndoKind {
    /// An in-place update, with the images of the columns it changed.
    Update,
    /// A delete, with no images. It holds the writer's stamp for the lock and garbage collection.
    Delete,
    /// A stub for records written to the spill file, see the module documentation. Its one image
    /// is where they are.
    Spilled,
}

impl UndoKind {
    fn code(self) -> u8 {
        match self {
            Self::Update => 0,
            Self::Delete => 1,
            Self::Spilled => 2,
        }
    }

    fn of(code: u8) -> Self {
        match code {
            1 => Self::Delete,
            2 => Self::Spilled,
            _ => Self::Update,
        }
    }
}

/// The value a column had before a change, `None` for null.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Image {
    /// The column.
    pub column: u16,
    /// The value, zero extended, or a text column's 16-byte view.
    pub value: Option<u128>,
}

type Chunk = Box<[AtomicU64]>;

/// 1,024 chunk ids, each set once.
type Directory = Box<[OnceLock<Chunk>]>;

/// The chunks every worker's undo records live in.
#[derive(Debug)]
pub struct UndoSpace {
    directories: Box<[OnceLock<Directory>]>,
    /// The next chunk id.
    chunks: AtomicU32,
    /// Chunks that were collected and are made again before a new one is.
    free: Mutex<Vec<u32>>,
    /// Chunks their workers filled, with the units used, which the next collection takes.
    sealed: Mutex<Vec<(u32, u32)>>,
    /// The collector's chunks between rounds.
    collector: Mutex<Rounds>,
    /// The spill file, which the collector writes and old snapshots read.
    spill: Mutex<Spill>,
    /// When the next [`UndoSpace::collect_due`] runs, in milliseconds.
    due: AtomicU64,
}

/// When and where undo goes to disk, `15-resources.md` section 15.3.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SpillLimits {
    /// The temp directory the file goes in.
    pub directory: PathBuf,
    /// Undo in memory past which the collector spills the oldest, in bytes, 25% of
    /// `memory_limit`.
    pub at: u64,
    /// The most the file may hold, in bytes, `max_temp_directory_size`.
    pub most: u64,
}

/// The spill file and what is in it.
#[derive(Debug, Default)]
struct Spill {
    limits: Option<SpillLimits>,
    file: Option<(PathBuf, File)>,
    /// Bytes written since the file last started again.
    written: u64,
    /// Stubs written and not yet freed. At none, nothing points into the file.
    stubs: u64,
}

/// A record read back from the spill file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Spilled {
    /// Its timestamp, a commit's or `ABORTED`.
    pub ts: u64,
    /// What it undoes.
    pub kind: UndoKind,
    /// The column images.
    pub images: Vec<Image>,
}

/// Sealed chunks by how far collection has got with them, see the module documentation.
#[derive(Debug, Default)]
struct Rounds {
    /// Sealed and not yet all below the horizon, with the units used.
    waiting: Vec<(u32, u32)>,
    /// All below the horizon at the epoch given.
    found: Vec<Held>,
    /// Out of every chain at the epoch given.
    cut: Vec<Held>,
    /// Records of waiting chunks taken out of their chains above the horizon, pruned or spilled.
    pruned: HashSet<UndoRef>,
    /// Where the collector writes: the stubs of spilled records and the records it moves.
    own: UndoBuffer,
}

/// A chunk on its way to the free list: the units used, the epoch of the step it took last, and
/// the tables its records belong to, whose statements are the only ones it waits for.
#[derive(Debug)]
struct Held {
    chunk: u32,
    units: u32,
    epoch: u64,
    tables: Vec<u32>,
}

/// What one [`UndoSpace::collect`] did.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Collected {
    /// Chunks found to hold only records nobody needs.
    pub found: usize,
    /// Records taken out of their chains.
    pub cut: usize,
    /// Chunks put on the free list.
    pub freed: usize,
    /// Records above the horizon taken out of their chains, since no snapshot reads them.
    pub pruned: usize,
    /// Records written to the spill file.
    pub spilled: usize,
    /// Records moved out of a chunk that held little else anyone needs.
    pub moved: usize,
    /// The snapshot killed because the spill file was full.
    pub killed: Option<Killed>,
}

/// Where the collector finds a record's chain, which is the hot stripe the rid names in the
/// table.
pub trait Heads {
    /// Takes the record at `record` out of the chain of `rid` in `table`, if the row's stripe is
    /// still hot and the record is still in its chain, and every record older than it as well
    /// when `older` says so.
    fn unlink(&self, table: u32, rid: u64, record: UndoRef, older: bool);

    /// Takes the records `spare` picks out of the chain of `rid` in `table`, each alone, if the
    /// row's stripe is still hot, in one walk down the chain, and returns them. `spare` is asked
    /// oldest first, each against the chain as the ones before it left it. The record at the head
    /// is not asked, and stays.
    fn prune(&self, table: u32, rid: u64, spare: &mut dyn FnMut(UndoRef) -> bool) -> Vec<UndoRef>;

    /// Puts `with` in the chain of `rid` in `table` where `record` is, with what `with` points at
    /// after it, if the row's stripe is still hot and the record is still in its chain, and says
    /// whether it did.
    fn replace(&self, table: u32, rid: u64, record: UndoRef, with: UndoRef) -> bool;

    /// The newest record in the chain of `rid` in `table`, or 0 when the row's stripe is not hot
    /// or the chain is empty.
    fn head(&self, table: u32, rid: u64) -> UndoRef;
}

impl Default for UndoSpace {
    fn default() -> Self {
        Self::new()
    }
}

impl UndoSpace {
    /// A space with no chunks.
    #[must_use]
    pub fn new() -> Self {
        Self {
            directories: (0..DIRECTORY).map(|_| OnceLock::new()).collect(),
            chunks: AtomicU32::new(1),
            free: Mutex::new(Vec::new()),
            sealed: Mutex::new(Vec::new()),
            collector: Mutex::new(Rounds::default()),
            spill: Mutex::new(Spill::default()),
            due: AtomicU64::new(0),
        }
    }

    /// Spills undo past `limits`, or never when `None`, which is how a space starts.
    pub fn spill_at(&self, limits: Option<SpillLimits>) {
        self.spill.lock().unwrap_or_else(PoisonError::into_inner).limits = limits;
    }

    /// Bytes of undo in memory: the chunks in use.
    #[must_use]
    pub fn bytes(&self) -> u64 {
        self.live_chunks() as u64 * CHUNK_BYTES
    }

    /// Bytes in the spill file.
    #[must_use]
    pub fn spilled_bytes(&self) -> u64 {
        self.spill.lock().unwrap_or_else(PoisonError::into_inner).written
    }

    /// Chunks made so far.
    #[must_use]
    pub fn chunks(&self) -> u32 {
        self.chunks.load(Ordering::Relaxed).min(1 << 20) - 1
    }

    /// Chunks collected and not yet made again.
    #[must_use]
    pub fn free_chunks(&self) -> usize {
        self.free.lock().unwrap_or_else(PoisonError::into_inner).len()
    }

    /// Chunks in use: made, and not on the free list.
    #[must_use]
    pub fn live_chunks(&self) -> usize {
        self.chunks() as usize - self.free_chunks()
    }

    /// [`Self::collect`] once its deadline has passed, as a worker calls it when a statement ends:
    /// the first call at or after the deadline collects and sets the next a millisecond on, and
    /// every other call returns `None` at once. `now` is in milliseconds of a clock that does not
    /// go back.
    pub fn collect_due(
        &self,
        now: u64,
        horizons: &Horizons,
        clock: &AtomicU64,
        heads: &dyn Heads,
    ) -> Option<Collected> {
        let due = self.due.load(Ordering::Relaxed);
        if now < due
            || self
                .due
                .compare_exchange(due, now + 1, Ordering::Relaxed, Ordering::Relaxed)
                .is_err()
        {
            return None;
        }
        Some(self.collect(horizons, clock, heads))
    }

    /// One round of garbage collection, `08-concurrency.md` section 8.9: moves each sealed chunk
    /// one step closer to the free list as the module documentation says, with the horizon of each
    /// table taken from `horizons` against `clock`, the last commit timestamp, and the chains
    /// found through `heads`.
    ///
    /// Only one collection runs at a time, and a call that finds another running returns at once.
    /// Every reader and writer of a chain has to be inside a statement `horizons` knows about, or a
    /// chunk can be made again under it.
    pub fn collect(&self, horizons: &Horizons, clock: &AtomicU64, heads: &dyn Heads) -> Collected {
        let Ok(mut rounds) = self.collector.try_lock() else { return Collected::default() };
        let rounds = &mut *rounds;
        let mut done = Collected::default();
        // Before any slot is read, so a statement the reads miss has a snapshot at or past it.
        let upto = clock.load(Ordering::SeqCst);
        rounds.waiting.append(&mut self.sealed.lock().unwrap_or_else(PoisonError::into_inner));

        let (ready, cut): (Vec<_>, Vec<_>) =
            rounds.cut.drain(..).partition(|held| horizons.passed(held.epoch, &held.tables));
        rounds.cut = cut;
        done.freed = ready.len();
        let stubs = ready
            .iter()
            .flat_map(|held| self.records(held.chunk, held.units))
            .filter(|(_, record)| record.kind() == UndoKind::Spilled)
            .count();
        if stubs > 0 {
            self.forget(stubs as u64);
        }
        self.free
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .extend(ready.into_iter().map(|held| held.chunk));

        let (ready, found): (Vec<_>, Vec<_>) =
            rounds.found.drain(..).partition(|held| horizons.passed(held.epoch, &held.tables));
        rounds.found = found;
        // A committed record at or below the horizon is where every reader stops, so the newest
        // of a row's is cut with every record older than it, and one walk down the chain does
        // for all of them. An aborted record can sit above records a reader still needs, so it is
        // taken out on its own.
        let mut rows: HashMap<(u32, u64), (u64, UndoRef)> = HashMap::new();
        let mut aborted = Vec::new();
        let mut cut = Vec::with_capacity(ready.len());
        for held in ready {
            for (at, record) in self.records(held.chunk, held.units) {
                // Out of its chain already, and nobody walks to it.
                if rounds.pruned.remove(&at) {
                    continue;
                }
                let (ts, row) = (record.ts(), (record.table(), record.rid()));
                if ts == ABORTED {
                    aborted.push((row, at));
                } else if rows.get(&row).is_none_or(|&(newest, _)| ts > newest) {
                    rows.insert(row, (ts, at));
                }
                done.cut += 1;
            }
            cut.push(held);
        }
        for ((table, rid), (_, at)) in rows {
            heads.unlink(table, rid, at, true);
        }
        for ((table, rid), at) in aborted {
            heads.unlink(table, rid, at, false);
        }

        let mut oldest = HashMap::new();
        let mut found = Vec::new();
        let pruned = &rounds.pruned;
        rounds.waiting.retain(|&(chunk, units)| {
            let mut tables = Vec::new();
            let below = self.records(chunk, units).all(|(at, record)| {
                let (ts, table) = (record.ts(), record.table());
                if !tables.contains(&table) {
                    tables.push(table);
                }
                ts == ABORTED
                    || pruned.contains(&at)
                    || (ts & UNCOMMITTED == 0
                        && ts
                            <= *oldest
                                .entry(table)
                                .or_insert_with(|| horizons.oldest(clock, table)))
            });
            if below {
                found.push(Held { chunk, units, epoch: 0, tables });
            }
            !below
        });
        done.found = found.len();

        // Then the records of the chunks still waiting that no snapshot falls under, a row's chain
        // at a time and oldest first, so each one is judged against the chain as the ones before
        // it left it.
        let committed = |ts: u64| ts & UNCOMMITTED == 0 && ts != ABORTED;
        let mut chains: HashMap<(u32, u64), HashSet<UndoRef>> = HashMap::new();
        for &(chunk, units) in &rounds.waiting {
            for (at, record) in self.records(chunk, units) {
                let ts = record.ts();
                if record.kind() == UndoKind::Update
                    && committed(ts)
                    && ts <= upto
                    && !rounds.pruned.contains(&at)
                {
                    chains.entry((record.table(), record.rid())).or_default().insert(at);
                }
            }
        }
        let mut snapshots = HashMap::new();
        for ((table, rid), asked) in chains {
            let held: &Vec<u64> =
                snapshots.entry(table).or_insert_with(|| horizons.snapshots(table));
            let taken = heads.prune(table, rid, &mut |at| {
                asked.contains(&at)
                    && self.record(at).is_some_and(|record| self.spare(record, upto, held))
            });
            done.pruned += taken.len();
            rounds.pruned.extend(taken);
        }

        self.spill_oldest(rounds, horizons, clock, heads, &mut done);
        self.compact(rounds, horizons, clock, heads, &mut done);
        // A copy in a chunk the collector has not sealed is held until it is, so once nothing is
        // waiting the chunk goes with the rest. Otherwise it fills first.
        if rounds.waiting.is_empty() {
            rounds.own.seal(self);
        }

        // After the cuts and the reads of every stamp above, so a statement that begins at this
        // epoch or later cannot reach a record cut here, and one that saw a value an abort took
        // back began before it.
        let epoch = horizons.advance();
        rounds.cut.extend(cut.into_iter().map(|held| Held { epoch, ..held }));
        rounds.found.extend(found.into_iter().map(|held| Held { epoch, ..held }));
        done
    }

    /// Moves the records left in each chunk still waiting, when they take an eighth of it or less,
    /// into the collector's own chunk, see the module documentation. A chunk with a record a
    /// writer has not committed yet stays, since the writer stamps the record where it is.
    fn compact(
        &self,
        rounds: &mut Rounds,
        horizons: &Horizons,
        clock: &AtomicU64,
        heads: &dyn Heads,
        done: &mut Collected,
    ) {
        let mut horizon = HashMap::new();
        let mut moving = Vec::new();
        for &(chunk, units) in &rounds.waiting {
            let mut left = Vec::new();
            let mut used = 0;
            let mut open = false;
            for (at, record) in self.records(chunk, units) {
                let ts = record.ts();
                if ts & UNCOMMITTED != 0 && ts != ABORTED {
                    open = true;
                    break;
                }
                let table = record.table();
                let below =
                    ts <= *horizon.entry(table).or_insert_with(|| horizons.oldest(clock, table));
                if ts == ABORTED || below || rounds.pruned.contains(&at) {
                    continue;
                }
                used += record.words.len().div_ceil(2) as u32;
                if used * 8 > units {
                    break;
                }
                left.push(at);
            }
            if !open && !left.is_empty() && used * 8 <= units {
                moving.extend(left);
            }
        }
        let mut stubs = 0;
        for at in moving {
            let Some(record) = self.record(at) else { continue };
            let row = Row { table: record.table(), rid: record.rid() };
            let images = record.images().collect::<Vec<_>>();
            let Some(copy) =
                rounds.own.one(self, record.kind(), record.ts(), row, record.prev(), &images)
            else {
                break;
            };
            stubs += u64::from(record.kind() == UndoKind::Spilled);
            if heads.replace(row.table, row.rid, at, copy) {
                rounds.pruned.insert(at);
                done.moved += 1;
            }
        }
        // Every stub is counted off when its chunk is freed, a copy too, put in its chain or not.
        if stubs > 0 {
            self.spill.lock().unwrap_or_else(PoisonError::into_inner).stubs += stubs;
        }
    }

    /// Writes the oldest undo still in chains to the spill file while there is more in memory than
    /// its limit, see the module documentation, or kills the oldest snapshot when the file is
    /// full.
    fn spill_oldest(
        &self,
        rounds: &mut Rounds,
        horizons: &Horizons,
        clock: &AtomicU64,
        heads: &dyn Heads,
        done: &mut Collected,
    ) {
        let mut spill = self.spill.lock().unwrap_or_else(PoisonError::into_inner);
        let Some(limits) = spill.limits.clone() else { return };
        let live = self.bytes();
        if live <= limits.at {
            return;
        }
        let chunks = (live - limits.at).div_ceil(CHUNK_BYTES) as usize;
        // The rows with a record in those chunks above its table's horizon. One at or below it is
        // about to be cut anyway.
        let committed = |ts: u64| ts & UNCOMMITTED == 0 && ts != ABORTED;
        let mut horizon = HashMap::new();
        let mut rows = Vec::new();
        let mut seen = HashSet::new();
        // Chunks whose records are all out of their chains already wait only for the epoch, and
        // do not count.
        let mut taken = 0;
        for &(chunk, units) in &rounds.waiting {
            if taken == chunks {
                break;
            }
            let before = rows.len();
            for (at, record) in self.records(chunk, units) {
                let (ts, table) = (record.ts(), record.table());
                let row = (table, record.rid());
                if !rounds.pruned.contains(&at)
                    && record.kind() != UndoKind::Spilled
                    && committed(ts)
                    && ts > *horizon.entry(table).or_insert_with(|| horizons.oldest(clock, table))
                    && seen.insert(row)
                {
                    rows.push(row);
                }
            }
            taken += usize::from(rows.len() > before);
        }
        // Each row's whole chain goes, from its newest committed record down, so the stub is all
        // that is left of it in memory. A record above that is a writer's that has not committed,
        // or an aborted one, and stays above the stub. A chain no bigger than a stub stays too.
        let mut bytes = Vec::new();
        for (table, rid) in rows {
            let mut next = heads.head(table, rid);
            let newest = loop {
                match self.record(next) {
                    Some(record) if committed(record.ts()) => break next,
                    Some(record) => next = record.prev(),
                    None => break 0,
                }
            };
            let Some(ts) = self.record(newest).map(|record| record.ts()) else { continue };
            if ts <= horizon[&table] {
                continue;
            }
            let (mut tail, mut words) = (Vec::new(), 0);
            let mut next = newest;
            while let Some(record) = self.record(next) {
                tail.push(next);
                words += record.words.len();
                encode(record, &mut bytes);
                next = record.prev();
            }
            if words <= HEADER_WORDS + IMAGE_WORDS {
                bytes.clear();
                continue;
            }
            let len = bytes.len() as u64;
            if spill.written + len > limits.most {
                done.killed = horizons.kill_oldest(clock, live + spill.written);
                break;
            }
            let Some(offset) = spill.append(&bytes) else { break };
            bytes.clear();
            let stub = Image { column: 0, value: Some(u128::from(offset) | u128::from(len) << 64) };
            let row = Row { table, rid };
            let Some(stub) = rounds.own.write(self, UndoKind::Spilled, ts, row, 0, &[stub]) else {
                break;
            };
            spill.stubs += 1;
            if heads.replace(table, rid, newest, stub) {
                done.spilled += tail.len();
                rounds.pruned.extend(tail);
            }
        }
        rounds.own.seal(self);
    }

    /// Notes that `stubs` stubs were freed, and starts the file again when none is left.
    fn forget(&self, stubs: u64) {
        let mut spill = self.spill.lock().unwrap_or_else(PoisonError::into_inner);
        spill.stubs = spill.stubs.saturating_sub(stubs);
        if spill.stubs == 0 {
            spill.written = 0;
            if let Some((_, file)) = &spill.file {
                let _ = file.set_len(0);
            }
        }
    }

    /// The records a stub stands for, newest first.
    ///
    /// # Panics
    ///
    /// If `stub` is not a stub, or the spill file cannot be read back, which leaves the reader
    /// with no way to see the row as its snapshot had it.
    #[must_use]
    pub fn unspill(&self, stub: Record<'_>) -> Vec<Spilled> {
        let image = stub.images().next().and_then(|image| image.value).expect("a stub's place");
        let (offset, len) = (image as u64, (image >> 64) as usize);
        let mut bytes = vec![0; len];
        let mut spill = self.spill.lock().unwrap_or_else(PoisonError::into_inner);
        let (_, file) = spill.file.as_mut().expect("a stub is in the file");
        file.seek(SeekFrom::Start(offset))
            .and_then(|_| file.read_exact(&mut bytes))
            .expect("the undo spill file reads back");
        drop(spill);
        decode(&bytes)
    }

    /// Whether `record` is a committed update no statement can read, though it is above its
    /// table's horizon: the write before it in the chain is a committed update of every column it
    /// has, at `P`, and none of `snapshots` is from `P` up to its own timestamp. Only a commit at or
    /// below `upto`, what the clock held before the snapshots were read, since a statement that
    /// begins later reads past it anyway.
    ///
    /// The write before it is the first record down the chain of another timestamp, past the other
    /// pieces of a wide update. An aborted one there is not passed, because a reader that stops at
    /// this record today would walk on into it.
    fn spare(&self, record: Record<'_>, upto: u64, snapshots: &[u64]) -> bool {
        let committed = |ts: u64| ts & UNCOMMITTED == 0 && ts != ABORTED;
        let ts = record.ts();
        if record.kind() != UndoKind::Update || !committed(ts) || ts > upto {
            return false;
        }
        let mut next = record.prev();
        let older = loop {
            let Some(older) = self.record(next) else { return false };
            if older.ts() != ts {
                break older;
            }
            next = older.prev();
        };
        let then = older.ts();
        if older.kind() != UndoKind::Update || !committed(then) || then >= ts {
            return false;
        }
        let first = snapshots.partition_point(|&snapshot| snapshot < then);
        if snapshots.get(first).is_some_and(|&snapshot| snapshot < ts) {
            return false;
        }
        let mut columns = Vec::new();
        let mut piece = Some(older);
        while let Some(record) = piece.filter(|record| record.ts() == then) {
            columns.extend(record.images().map(|image| image.column));
            piece = self.record(record.prev());
        }
        record.images().all(|image| columns.contains(&image.column))
    }

    /// The records in the first `units` units of `chunk`, oldest first.
    fn records(&self, chunk: u32, units: u32) -> impl Iterator<Item = (UndoRef, Record<'_>)> {
        let mut next = 0;
        std::iter::from_fn(move || {
            if next >= units {
                return None;
            }
            let at = (chunk << 12) | next;
            let record = self.record(at)?;
            next += record.words.len().div_ceil(2) as u32;
            Some((at, record))
        })
    }

    /// The record at `at`, which a release store published, or `None` for 0 and for a chunk that
    /// was never made.
    #[must_use]
    pub fn record(&self, at: UndoRef) -> Option<Record<'_>> {
        let words = self.words(at >> 12)?.get((at & 0xFFF) as usize * 2..)?;
        let images = ((words[0].load(Ordering::Relaxed) >> 40) & 0xFFFF) as usize;
        let len = HEADER_WORDS + IMAGE_WORDS * images;
        Some(Record { words: words.get(..len)? })
    }

    /// A chunk's id, one collected if there is one and a new one if not, or `None` when all 2^20
    /// are made and none is free.
    fn make(&self) -> Option<u32> {
        if let Some(id) = self.free.lock().unwrap_or_else(PoisonError::into_inner).pop() {
            return Some(id);
        }
        let id = self.chunks.fetch_add(1, Ordering::Relaxed);
        if id >= 1 << 20 {
            return None;
        }
        let directory = self.directories[id as usize / DIRECTORY]
            .get_or_init(|| (0..DIRECTORY).map(|_| OnceLock::new()).collect());
        // The id came from a `fetch_add`, so nobody else sets this entry.
        let _ = directory[id as usize % DIRECTORY]
            .set((0..CHUNK_WORDS).map(|_| AtomicU64::new(0)).collect());
        Some(id)
    }

    fn words(&self, chunk: u32) -> Option<&[AtomicU64]> {
        let directory = self.directories.get(chunk as usize / DIRECTORY)?.get()?;
        directory[chunk as usize % DIRECTORY].get().map(|words| &words[..])
    }
}

impl Drop for UndoSpace {
    fn drop(&mut self) {
        let spill = self.spill.get_mut().unwrap_or_else(PoisonError::into_inner);
        if let Some((path, file)) = spill.file.take() {
            drop(file);
            let _ = std::fs::remove_file(path);
        }
    }
}

impl Spill {
    /// Writes `bytes` at the end of the file, made in the temp directory the first time, and
    /// returns where. `None` when the file cannot be made or written, and then nothing spills.
    fn append(&mut self, bytes: &[u8]) -> Option<u64> {
        static FILES: AtomicU64 = AtomicU64::new(0);
        if self.file.is_none() {
            let directory = &self.limits.as_ref()?.directory;
            std::fs::create_dir_all(directory).ok()?;
            let name = format!(
                "rudb-undo-{}-{}.tmp",
                std::process::id(),
                FILES.fetch_add(1, Ordering::Relaxed)
            );
            let path = directory.join(name);
            let file = File::options()
                .read(true)
                .write(true)
                .create(true)
                .truncate(true)
                .open(&path)
                .ok()?;
            self.file = Some((path, file));
        }
        let (_, file) = self.file.as_mut()?;
        let offset = self.written;
        file.seek(SeekFrom::Start(offset)).and_then(|_| file.write_all(bytes)).ok()?;
        self.written += bytes.len() as u64;
        Some(offset)
    }
}

/// Appends `record` as the spill file holds it: the timestamp, the kind, the image count, then
/// each image's column, validity and value, little endian.
fn encode(record: Record<'_>, out: &mut Vec<u8>) {
    out.extend_from_slice(&record.ts().to_le_bytes());
    out.push(record.kind().code());
    let images = record.images().collect::<Vec<_>>();
    out.extend_from_slice(&(images.len() as u16).to_le_bytes());
    for image in images {
        out.extend_from_slice(&image.column.to_le_bytes());
        out.push(u8::from(image.value.is_some()));
        out.extend_from_slice(&image.value.unwrap_or(0).to_le_bytes());
    }
}

/// The records [`encode`] wrote into `bytes`.
fn decode(mut bytes: &[u8]) -> Vec<Spilled> {
    fn take<'a>(bytes: &mut &'a [u8], n: usize) -> &'a [u8] {
        let (head, rest) = bytes.split_at(n);
        *bytes = rest;
        head
    }
    let mut records = Vec::new();
    while !bytes.is_empty() {
        let ts = u64::from_le_bytes(take(&mut bytes, 8).try_into().expect("eight bytes"));
        let kind = UndoKind::of(take(&mut bytes, 1)[0]);
        let count = u16::from_le_bytes(take(&mut bytes, 2).try_into().expect("two bytes"));
        let images = (0..count)
            .map(|_| {
                let column = u16::from_le_bytes(take(&mut bytes, 2).try_into().expect("two"));
                let valid = take(&mut bytes, 1)[0] != 0;
                let value = u128::from_le_bytes(take(&mut bytes, 16).try_into().expect("16"));
                Image { column, value: valid.then_some(value) }
            })
            .collect();
        records.push(Spilled { ts, kind, images });
    }
    records
}

/// One undo record, read in place.
#[derive(Debug, Clone, Copy)]
pub struct Record<'a> {
    words: &'a [AtomicU64],
}

impl<'a> Record<'a> {
    /// The next older record of the same row.
    #[must_use]
    pub fn prev(&self) -> UndoRef {
        self.words[0].load(Ordering::Acquire) as u32
    }

    /// Points it at `prev` instead, which is how the collector takes the record after it out of
    /// the chain or puts a stub there, and how a writer that lost a race for the row's head moves
    /// a record it has not published yet onto the new one. A release, since a stub is published
    /// by it.
    pub(crate) fn set_prev(&self, prev: UndoRef) {
        let _ = self.words[0].fetch_update(Ordering::Release, Ordering::Relaxed, |word| {
            Some((word & !u64::from(u32::MAX)) | u64::from(prev))
        });
    }

    /// What it undoes.
    #[must_use]
    pub fn kind(&self) -> UndoKind {
        UndoKind::of((self.words[0].load(Ordering::Relaxed) >> 32) as u8)
    }

    /// The writer's transaction id with the top bit set until it commits, then its commit
    /// timestamp.
    #[must_use]
    pub fn ts(&self) -> u64 {
        self.words[1].load(Ordering::Acquire)
    }

    /// Stamps it, at commit or abort.
    pub fn stamp(&self, ts: u64) {
        self.words[1].store(ts, Ordering::Release);
    }

    /// The row, so garbage collection can find the chain's head.
    #[must_use]
    pub fn rid(&self) -> u64 {
        self.words[2].load(Ordering::Relaxed)
    }

    /// The row's table, so garbage collection can hold it to the table's horizon.
    #[must_use]
    pub fn table(&self) -> u32 {
        self.words[3].load(Ordering::Relaxed) as u32
    }

    /// The column images.
    pub fn images(&self) -> impl Iterator<Item = Image> + 'a {
        self.words[HEADER_WORDS..].chunks_exact(IMAGE_WORDS).map(|image| {
            let head = image[0].load(Ordering::Relaxed);
            let value = u128::from(image[1].load(Ordering::Relaxed))
                | (u128::from(image[2].load(Ordering::Relaxed)) << 64);
            Image { column: head as u16, value: (head & (1 << 16) != 0).then_some(value) }
        })
    }
}

/// A worker's run of undo space: units `next..` of `chunk` are its own, and chunk 0 is none yet.
#[derive(Debug, Default)]
pub struct UndoBuffer {
    chunk: u32,
    next: u32,
}

/// Where a record belongs: the row and its table.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Row {
    /// The table, whose horizon the record is held to.
    pub table: u32,
    /// The row, see `Rid::get`.
    pub rid: u64,
}

impl UndoBuffer {
    /// Writes a record of `kind` for `row` stamped `ts`, after `prev`, and returns its reference,
    /// not yet published. An update of more than [`MOST_IMAGES`] columns is written as a chain of
    /// records, and the reference is the newest.
    ///
    /// `None` once the space has made all its chunks and none is free.
    pub fn write(
        &mut self,
        space: &UndoSpace,
        kind: UndoKind,
        ts: u64,
        row: Row,
        mut prev: UndoRef,
        images: &[Image],
    ) -> Option<UndoRef> {
        let mut pieces = images.chunks(MOST_IMAGES);
        let first = pieces.next().unwrap_or_default();
        for piece in std::iter::once(first).chain(pieces) {
            prev = self.one(space, kind, ts, row, prev, piece)?;
        }
        Some(prev)
    }

    /// Hands the chunk it is writing in to the collector, as a worker does before it goes away.
    /// The next record starts a chunk of its own.
    pub fn seal(&mut self, space: &UndoSpace) {
        if self.chunk != 0 && self.next > 0 {
            space
                .sealed
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .push((self.chunk, self.next));
        }
        self.chunk = 0;
        self.next = 0;
    }

    fn one(
        &mut self,
        space: &UndoSpace,
        kind: UndoKind,
        ts: u64,
        row: Row,
        prev: UndoRef,
        images: &[Image],
    ) -> Option<UndoRef> {
        let len = HEADER_WORDS + IMAGE_WORDS * images.len();
        let units = len.div_ceil(2) as u32;
        if self.chunk == 0 || self.next + units > (CHUNK_WORDS / 2) as u32 {
            self.seal(space);
            self.chunk = space.make()?;
            self.next = 0;
        }
        let at = (self.chunk << 12) | self.next;
        let start = self.next as usize * 2;
        let record = &space.words(self.chunk)?[start..start + len];
        let kind = u64::from(kind.code());
        record[0].store(
            u64::from(prev) | (kind << 32) | ((images.len() as u64) << 40),
            Ordering::Relaxed,
        );
        record[1].store(ts, Ordering::Relaxed);
        record[2].store(row.rid, Ordering::Relaxed);
        record[3].store(u64::from(row.table), Ordering::Relaxed);
        for (image, out) in images.iter().zip(record[HEADER_WORDS..].chunks_exact(IMAGE_WORDS)) {
            let valid = u64::from(image.value.is_some()) << 16;
            let value = image.value.unwrap_or(0);
            out[0].store(u64::from(image.column) | valid, Ordering::Relaxed);
            out[1].store(value as u64, Ordering::Relaxed);
            out[2].store((value >> 64) as u64, Ordering::Relaxed);
        }
        self.next += units;
        Some(at)
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;
    use std::thread;

    use super::{Image, MOST_IMAGES, Row, UndoBuffer, UndoKind, UndoSpace};

    fn row(rid: u64) -> Row {
        Row { table: 0, rid }
    }

    fn images(n: usize, seed: u128) -> Vec<Image> {
        (0..n)
            .map(|i| Image {
                column: i as u16,
                value: (i % 3 != 0).then_some(seed.wrapping_mul(i as u128 + 1) << 60),
            })
            .collect()
    }

    #[test]
    fn a_record_reads_back_as_written() {
        let space = UndoSpace::new();
        let mut buffer = UndoBuffer::default();
        assert!(space.record(0).is_none(), "0 is no record");
        let first = buffer.write(&space, UndoKind::Update, 7 | 1 << 63, row(42), 0, &images(4, 9));
        let first = first.expect("room");
        let second = buffer.write(&space, UndoKind::Delete, 8 | 1 << 63, row(42), first, &[]);
        let second = second.expect("room");
        let record = space.record(second).expect("written");
        assert_eq!(record.kind(), UndoKind::Delete);
        assert_eq!(record.prev(), first);
        assert_eq!((record.rid(), record.table()), (42, 0));
        assert_eq!(record.images().count(), 0);
        let record = space.record(record.prev()).expect("written");
        assert_eq!(record.kind(), UndoKind::Update);
        assert_eq!(record.ts(), 7 | 1 << 63);
        record.stamp(100);
        assert_eq!(record.ts(), 100);
        assert_eq!(record.images().collect::<Vec<_>>(), images(4, 9));
        assert_eq!(record.prev(), 0);
    }

    #[test]
    fn records_move_to_a_new_chunk_and_wide_updates_split() {
        let space = UndoSpace::new();
        let mut buffer = UndoBuffer::default();
        let mut refs = Vec::new();
        for i in 0..3_000 {
            refs.push(buffer.write(&space, UndoKind::Update, i, row(i), 0, &images(5, i.into())));
        }
        assert!(space.chunks() > 1, "3,000 records of 144 bytes are more than 64 KiB");
        for (i, at) in refs.into_iter().enumerate() {
            let record = space.record(at.expect("room")).expect("written");
            assert_eq!(record.ts(), i as u64);
            assert_eq!(record.images().collect::<Vec<_>>(), images(5, i as u128));
        }
        let wide = images(MOST_IMAGES * 2 + 10, 3);
        let mut at = buffer.write(&space, UndoKind::Update, 1, row(1), 0, &wide).expect("room");
        let mut back = Vec::new();
        while let Some(record) = space.record(at) {
            let mut piece: Vec<_> = record.images().collect();
            piece.extend(back);
            back = piece;
            at = record.prev();
        }
        assert_eq!(back, wide, "the chain holds every image once, oldest record first");
    }

    #[test]
    fn workers_write_into_their_own_buffers() {
        let space = Arc::new(UndoSpace::new());
        let workers: Vec<_> = (0..8_u64)
            .map(|worker| {
                let space = Arc::clone(&space);
                thread::spawn(move || {
                    let mut buffer = UndoBuffer::default();
                    (0..10_000_u64)
                        .map(|i| {
                            let image = [Image { column: 1, value: Some(u128::from(i)) }];
                            buffer.write(&space, UndoKind::Update, worker, row(i), 0, &image)
                        })
                        .collect::<Vec<_>>()
                })
            })
            .collect();
        for (worker, handle) in workers.into_iter().enumerate() {
            for (i, at) in handle.join().expect("the worker finishes").into_iter().enumerate() {
                let record = space.record(at.expect("room")).expect("written");
                assert_eq!((record.ts(), record.rid()), (worker as u64, i as u64));
                let image = record.images().next().expect("one image");
                assert_eq!(image.value, Some(i as u128));
            }
        }
    }

    mod collection {
        use std::path::{Path, PathBuf};
        use std::sync::atomic::{AtomicU64, Ordering};
        use std::sync::{Arc, Barrier, Mutex};
        use std::thread;
        use std::time::Instant;

        use crate::horizon::{Horizons, Reads};
        use crate::hot::Width;
        use crate::park::Wait;
        use crate::stripes::{Inserter, Rid, Stripe, StripeDirectory};
        use crate::undo::{Heads, SpillLimits, UndoBuffer, UndoRef, UndoSpace};

        /// Several tables, as the database finds a record's chain.
        struct Tables(Vec<Arc<StripeDirectory>>);

        impl Heads for Tables {
            fn unlink(&self, table: u32, rid: u64, record: UndoRef, older: bool) {
                for directory in &self.0 {
                    directory.unlink(table, rid, record, older);
                }
            }

            fn prune(
                &self,
                table: u32,
                rid: u64,
                spare: &mut dyn FnMut(UndoRef) -> bool,
            ) -> Vec<UndoRef> {
                for directory in &self.0 {
                    let taken = directory.prune(table, rid, &mut *spare);
                    if !taken.is_empty() {
                        return taken;
                    }
                }
                Vec::new()
            }

            fn replace(&self, table: u32, rid: u64, record: UndoRef, with: UndoRef) -> bool {
                self.0.iter().any(|directory| directory.replace(table, rid, record, with))
            }

            fn head(&self, table: u32, rid: u64) -> UndoRef {
                self.0.iter().map(|directory| directory.head(table, rid)).max().unwrap_or(0)
            }
        }

        fn table(space: &Arc<UndoSpace>, id: u32) -> Arc<StripeDirectory> {
            Arc::new(StripeDirectory::new(&[Width::Eight], Arc::clone(space)).in_table(id))
        }

        /// Rows of value 0 in `table`, committed by transaction 1 at timestamp 1, a lease at a
        /// time.
        fn rows(table: &Arc<StripeDirectory>, n: u32) -> Vec<Rid> {
            let mut inserter = Inserter::new(Arc::clone(table));
            let mut rids = Vec::new();
            while rids.len() < n as usize {
                let (lease, slots) = inserter.take(n - rids.len() as u32);
                for slot in slots.clone() {
                    lease.stripe().write(slot, 0, Some(0));
                }
                lease.stripe().insert(slots.clone(), 1);
                lease.stripe().commit_insert(slots.clone(), 1, 1);
                rids.extend(inserter.rids(slots));
            }
            rids
        }

        fn as_of(table: &StripeDirectory, rid: Rid, snapshot: u64) -> Option<u128> {
            let Some(Stripe::Hot(stripe)) = table.find(rid.stripe()) else { panic!("hot") };
            let mut out = [None];
            stripe.read_as_of(rid.slot(), &[0], snapshot, 0, &mut out);
            out[0]
        }

        /// Updates `rid` to `ts` and commits it at `ts`, `n` times from timestamp `clock + 1` on.
        fn updates(
            table: &StripeDirectory,
            rid: Rid,
            clock: &AtomicU64,
            n: u64,
            buffer: &mut UndoBuffer,
        ) {
            let Some(Stripe::Hot(stripe)) = table.find(rid.stripe()) else { panic!("hot") };
            for _ in 0..n {
                let snapshot = clock.load(Ordering::SeqCst);
                let ts = snapshot + 1;
                let value = Some(u128::from(ts));
                stripe
                    .update(rid.slot(), &[(0, value)], snapshot, ts, Wait::NEVER, buffer)
                    .expect("nobody else writes it");
                stripe.commit_write(rid.slot(), ts, ts);
                clock.store(ts, Ordering::SeqCst);
            }
        }

        #[test]
        fn chunks_below_the_horizon_are_freed_in_three_rounds_and_made_again() {
            let space = Arc::new(UndoSpace::new());
            let items = table(&space, 1);
            let rid = rows(&items, 1)[0];
            let clock = AtomicU64::new(1);
            let horizons = Horizons::new(1);
            let heads = Tables(vec![Arc::clone(&items)]);
            let mut buffer = UndoBuffer::default();
            updates(&items, rid, &clock, 5_000, &mut buffer);
            let made = space.chunks();
            assert!(made >= 4, "5,000 records of 64 bytes fill four chunks and more");

            let first = space.collect(&horizons, &clock, &heads);
            assert_eq!((first.found, first.cut, first.freed), (made as usize - 1, 0, 0));
            let second = space.collect(&horizons, &clock, &heads);
            assert_eq!((second.found, second.freed), (0, 0));
            assert!(second.cut > 4_000, "{second:?}");
            let third = space.collect(&horizons, &clock, &heads);
            assert_eq!(third.freed, made as usize - 1);
            assert_eq!(space.live_chunks(), 1, "only the chunk the buffer is still writing");

            let now = clock.load(Ordering::SeqCst);
            assert_eq!(as_of(&items, rid, now), Some(u128::from(now)));
            updates(&items, rid, &clock, 3_000, &mut buffer);
            assert_eq!(space.chunks(), made, "the freed chunks were made again");
            let now = clock.load(Ordering::SeqCst);
            assert_eq!(as_of(&items, rid, now), Some(u128::from(now)));
            assert_eq!(as_of(&items, rid, now - 100), Some(u128::from(now - 100)));
        }

        #[test]
        fn an_old_snapshot_keeps_its_table_and_not_the_one_beside_it() {
            let space = Arc::new(UndoSpace::new());
            let (orders, stock) = (table(&space, 1), table(&space, 2));
            let (order, item) = (rows(&orders, 1)[0], rows(&stock, 1)[0]);
            let clock = AtomicU64::new(1);
            let horizons = Horizons::new(2);
            let heads = Tables(vec![Arc::clone(&orders), Arc::clone(&stock)]);
            let old = horizons.begin(0, &clock, Reads::Tables(vec![1]));
            let (mut first, mut second) = (UndoBuffer::default(), UndoBuffer::default());
            updates(&orders, order, &clock, 10, &mut first);
            let later = horizons.begin(1, &clock, Reads::Tables(vec![1]));
            updates(&orders, order, &clock, 2_990, &mut first);
            updates(&stock, item, &clock, 3_000, &mut second);
            first.seal(&space);
            second.seal(&space);
            let made = space.chunks() as usize;

            let mut pruned = 0;
            for _ in 0..5 {
                pruned += space.collect(&horizons, &clock, &heads).pruned;
            }
            // Every chunk of `stock` is free. The records of `orders` between the one each
            // snapshot reads and the row's newest are out of the chain, but the two statements
            // began before they were taken out and may hold a reference to one, so their chunks
            // wait for them.
            assert!(pruned > 2_900, "{pruned} pruned");
            let kept = space.live_chunks();
            assert!(kept < made && kept >= made / 2 - 1, "{kept} of {made}");
            assert_eq!(as_of(&orders, order, old), Some(0), "the old snapshot still reads");
            assert_eq!(as_of(&orders, order, later), Some(u128::from(later)));

            // Each transaction goes on with statements at the snapshot it has. Those begin after
            // the records were taken out, so of `orders` only the records the two snapshots read
            // and the row's newest are left, which the collector moved into a chunk of its own.
            for _ in 0..3 {
                horizons.end(0);
                horizons.end(1);
                horizons.resume(0, old);
                horizons.resume(1, later);
                space.collect(&horizons, &clock, &heads);
            }
            assert_eq!(space.live_chunks(), 1, "{made} made");
            assert_eq!(as_of(&orders, order, old), Some(0), "the old snapshot still reads");
            assert_eq!(as_of(&orders, order, later), Some(u128::from(later)));

            horizons.end(0);
            horizons.end(1);
            for _ in 0..3 {
                space.collect(&horizons, &clock, &heads);
            }
            assert_eq!(space.live_chunks(), 0);
            let now = clock.load(Ordering::SeqCst);
            assert_eq!(as_of(&orders, order, now), Some(3_001));
            assert_eq!(as_of(&stock, item, now), Some(u128::from(now)));
        }

        #[test]
        fn an_aborted_record_waits_for_the_statements_running_at_the_abort() {
            let space = Arc::new(UndoSpace::new());
            let items = table(&space, 1);
            let rid = rows(&items, 1)[0];
            let clock = AtomicU64::new(1);
            let horizons = Horizons::new(2);
            let heads = Tables(vec![Arc::clone(&items)]);
            let Some(Stripe::Hot(stripe)) = items.find(rid.stripe()) else { panic!("hot") };
            horizons.begin(0, &clock, Reads::Tables(vec![1]));
            // Running for the whole test, on a table none of the records belong to.
            horizons.begin(1, &clock, Reads::Tables(vec![2]));
            let mut buffer = UndoBuffer::default();
            for txn in 2..1_500 {
                stripe
                    .update(rid.slot(), &[(0, Some(9))], 1, txn, Wait::NEVER, &mut buffer)
                    .expect("free");
                stripe.abort_write(rid.slot(), txn);
            }
            buffer.seal(&space);
            let made = space.chunks() as usize;
            assert_eq!(space.collect(&horizons, &clock, &heads).found, made);
            for _ in 0..3 {
                let round = space.collect(&horizons, &clock, &heads);
                assert_eq!((round.cut, round.freed), (0, 0), "a statement from before is running");
            }
            horizons.end(0);
            assert_eq!(space.collect(&horizons, &clock, &heads).cut, 1_498);
            assert_eq!(space.collect(&horizons, &clock, &heads).freed, made);
            assert_eq!(as_of(&items, rid, 1), Some(0));
        }

        #[test]
        fn a_record_no_snapshot_reads_is_pruned_above_the_horizon() {
            let space = Arc::new(UndoSpace::new());
            let items = table(&space, 1);
            let rid = rows(&items, 1)[0];
            let clock = AtomicU64::new(1);
            let horizons = Horizons::new(2);
            let heads = Tables(vec![Arc::clone(&items)]);
            let mut buffer = UndoBuffer::default();
            let old = horizons.begin(0, &clock, Reads::Tables(vec![1]));
            updates(&items, rid, &clock, 100, &mut buffer);
            let later = horizons.begin(1, &clock, Reads::Tables(vec![1]));
            updates(&items, rid, &clock, 100, &mut buffer);
            buffer.seal(&space);
            // Kept are the first update, which nothing older is under, the one over the update
            // the later snapshot reads, and the newest.
            // The three are all that is left of the chunk, and move out of it.
            let round = space.collect(&horizons, &clock, &heads);
            assert_eq!((round.found, round.pruned, round.moved), (0, 197, 3));
            assert_eq!(space.collect(&horizons, &clock, &heads).pruned, 0, "pruned once");
            let now = clock.load(Ordering::SeqCst);
            assert_eq!(as_of(&items, rid, old), Some(0));
            assert_eq!(as_of(&items, rid, later), Some(u128::from(later)));
            assert_eq!(as_of(&items, rid, now), Some(u128::from(now)));
            assert_eq!(as_of(&items, rid, now - 1), Some(u128::from(now - 1)));

            // Once both statements end, the rest are below the horizon and the chunk goes.
            horizons.end(0);
            horizons.end(1);
            for _ in 0..3 {
                space.collect(&horizons, &clock, &heads);
            }
            assert_eq!(space.live_chunks(), 0);
            assert_eq!(as_of(&items, rid, now), Some(u128::from(now)));
        }

        #[test]
        fn a_record_is_pruned_only_over_one_with_every_column_it_has() {
            let space = Arc::new(UndoSpace::new());
            let items = Arc::new(
                StripeDirectory::new(&[Width::Eight, Width::Eight], Arc::clone(&space)).in_table(1),
            );
            let rid = rows(&items, 1)[0];
            let Some(Stripe::Hot(stripe)) = items.find(rid.stripe()) else { panic!("hot") };
            stripe.write(rid.slot(), 1, Some(0));
            let clock = AtomicU64::new(1);
            let horizons = Horizons::new(1);
            let heads = Tables(vec![Arc::clone(&items)]);
            let mut buffer = UndoBuffer::default();
            let old = horizons.begin(0, &clock, Reads::Tables(vec![1]));
            // Columns 1, 0, 0, 1, 1 at timestamps 2 to 6. Only the second update of column 0 is
            // over one of the same column with no snapshot between. The second of column 1 is
            // the newest, and the first is over an update of column 0.
            for (ts, column) in [(2_u64, 1_usize), (3, 0), (4, 0), (5, 1), (6, 1)] {
                stripe
                    .update(
                        rid.slot(),
                        &[(column, Some(u128::from(ts)))],
                        ts - 1,
                        ts,
                        Wait::NEVER,
                        &mut buffer,
                    )
                    .expect("nobody else writes it");
                stripe.commit_write(rid.slot(), ts, ts);
                clock.store(ts, Ordering::SeqCst);
            }
            buffer.seal(&space);
            assert_eq!(space.collect(&horizons, &clock, &heads).pruned, 1);
            let read = |snapshot| {
                let mut out = [None, None];
                stripe.read_as_of(rid.slot(), &[0, 1], snapshot, 0, &mut out);
                out
            };
            assert_eq!(read(old), [Some(0), Some(0)]);
            assert_eq!(read(6), [Some(4), Some(6)]);
            horizons.end(0);
        }

        #[test]
        fn the_few_records_an_old_snapshot_reads_move_out_and_let_their_chunks_go() {
            let space = Arc::new(UndoSpace::new());
            let items = table(&space, 1);
            let rids = rows(&items, 8);
            let clock = AtomicU64::new(1);
            let horizons = Horizons::new(1);
            let heads = Tables(vec![Arc::clone(&items)]);
            let mut buffer = UndoBuffer::default();
            let old = horizons.begin(0, &clock, Reads::Tables(vec![1]));
            for &rid in &rids {
                updates(&items, rid, &clock, 1_000, &mut buffer);
            }
            buffer.seal(&space);
            assert_eq!(space.chunks(), 8);

            // Of each row's thousand records the old snapshot reads the first, and the newest is
            // at the head. The rest are pruned, and the two left in each chunk or so move out.
            let round = space.collect(&horizons, &clock, &heads);
            assert_eq!((round.pruned, round.moved), (8 * 998, 16));
            for _ in 0..3 {
                horizons.end(0);
                horizons.resume(0, old);
                space.collect(&horizons, &clock, &heads);
            }
            assert_eq!(space.live_chunks(), 1, "the collector's own, with the sixteen");
            let now = clock.load(Ordering::SeqCst);
            for (row, &rid) in rids.iter().enumerate() {
                let last = old + 1_000 * (row as u64 + 1);
                assert_eq!(
                    as_of(&items, rid, old),
                    Some(0),
                    "row {row} as the old snapshot reads it"
                );
                assert_eq!(as_of(&items, rid, now), Some(u128::from(last)), "row {row} now");
            }

            horizons.end(0);
            for _ in 0..3 {
                space.collect(&horizons, &clock, &heads);
            }
            assert_eq!(space.live_chunks(), 0);
            for (row, &rid) in rids.iter().enumerate() {
                assert_eq!(
                    as_of(&items, rid, now),
                    Some(u128::from(old + 1_000 * (row as u64 + 1)))
                );
            }
        }

        #[test]
        fn a_collection_is_due_once_a_millisecond() {
            let space = UndoSpace::new();
            let clock = AtomicU64::new(1);
            let horizons = Horizons::new(1);
            let heads = Tables(Vec::new());
            let due = |now| space.collect_due(now, &horizons, &clock, &heads).is_some();
            assert!(due(5));
            assert!(!due(5), "the next is at 6");
            assert!(!due(4));
            assert!(due(6));
            assert!(due(9));
            assert!(!due(9));
        }

        fn spill_directory(tag: &str) -> PathBuf {
            let directory =
                std::env::temp_dir().join(format!("rudb-undo-spill-{tag}-{}", std::process::id()));
            let _ = std::fs::remove_dir_all(&directory);
            directory
        }

        /// The length of every file in `directory`.
        fn spill_files(directory: &Path) -> Vec<u64> {
            std::fs::read_dir(directory).map_or_else(
                |_| Vec::new(),
                |entries| {
                    entries
                        .map(|entry| {
                            entry.and_then(|entry| entry.metadata()).expect("a file").len()
                        })
                        .collect()
                },
            )
        }

        #[test]
        fn undo_an_old_snapshot_holds_goes_to_the_spill_file_and_reads_back() {
            let directory = spill_directory("reads");
            let space = Arc::new(UndoSpace::new());
            let limits =
                SpillLimits { directory: directory.clone(), at: 4 * 65_536, most: 1 << 30 };
            space.spill_at(Some(limits));
            let items = table(&space, 1);
            let rids = rows(&items, 2_000);
            let clock = AtomicU64::new(1);
            let horizons = Horizons::new(1);
            let heads = Tables(vec![Arc::clone(&items)]);
            let mut buffer = UndoBuffer::default();
            let old = horizons.begin(0, &clock, Reads::Tables(vec![1]));
            for &rid in &rids {
                updates(&items, rid, &clock, 3, &mut buffer);
            }
            buffer.seal(&space);
            let made = space.chunks() as usize;
            assert!(made > 4, "{made} chunks");

            // Each row's second record is pruned, since nobody reads between its first and its
            // third. The third, at the head, and the first, which the old snapshot reads, go to
            // the file, and a stub stands for the two.
            let (mut pruned, mut spilled) = (0, 0);
            for _ in 0..4 {
                let round = space.collect(&horizons, &clock, &heads);
                (pruned, spilled) = (pruned + round.pruned, spilled + round.spilled);
                assert_eq!(round.killed, None);
            }
            assert_eq!((pruned, spilled), (2_000, 4_000));
            assert!(space.spilled_bytes() > 0);
            assert_eq!(spill_files(&directory), vec![space.spilled_bytes()]);
            let now = clock.load(Ordering::SeqCst);
            for (row, &rid) in rids.iter().enumerate() {
                assert_eq!(
                    as_of(&items, rid, old),
                    Some(0),
                    "row {row} as the old snapshot reads it"
                );
                let last = old + 3 * (row as u64 + 1);
                assert_eq!(as_of(&items, rid, now), Some(u128::from(last)), "row {row} now");
                assert_eq!(as_of(&items, rid, last - 1), Some(u128::from(last - 1)));
            }

            // The statements after the first at the same snapshot let the chunks go, and what is
            // left in memory is the stubs, under the limit.
            for _ in 0..3 {
                horizons.end(0);
                horizons.resume(0, old);
                space.collect(&horizons, &clock, &heads);
            }
            assert!(space.bytes() <= 4 * 65_536, "{} bytes of undo in memory", space.bytes());
            for &rid in &rids {
                assert_eq!(as_of(&items, rid, old), Some(0));
            }

            // Once the snapshot is gone, so are the stubs, and the file starts again.
            horizons.end(0);
            for _ in 0..4 {
                space.collect(&horizons, &clock, &heads);
            }
            assert_eq!(space.live_chunks(), 0);
            assert_eq!(space.spilled_bytes(), 0);
            assert_eq!(spill_files(&directory), vec![0]);
            for (row, &rid) in rids.iter().enumerate() {
                let last = old + 3 * (row as u64 + 1);
                assert_eq!(as_of(&items, rid, now), Some(u128::from(last)));
            }
            drop((heads, items, space));
            assert_eq!(spill_files(&directory), Vec::<u64>::new(), "the file goes with the space");
        }

        #[test]
        fn a_full_spill_file_kills_the_oldest_snapshot_and_its_undo_goes() {
            let directory = spill_directory("full");
            let space = Arc::new(UndoSpace::new());
            let limits = SpillLimits { directory: directory.clone(), at: 65_536, most: 10_000 };
            space.spill_at(Some(limits));
            let items = table(&space, 1);
            let rids = rows(&items, 2_000);
            let clock = AtomicU64::new(1);
            let horizons = Horizons::new(2);
            let heads = Tables(vec![Arc::clone(&items)]);
            let mut buffer = UndoBuffer::default();
            let old = horizons.begin(0, &clock, Reads::Tables(vec![1]));
            for &rid in &rids {
                updates(&items, rid, &clock, 3, &mut buffer);
            }
            buffer.seal(&space);
            let later = horizons.begin(1, &clock, Reads::Tables(vec![1]));

            let round = space.collect(&horizons, &clock, &heads);
            let killed = round.killed.expect("the file filled");
            assert_eq!((killed.snapshot, killed.behind), (old, 6_000));
            assert!(round.spilled > 0 && space.spilled_bytes() <= 10_000, "{round:?}");
            assert_eq!(horizons.too_old(0), Some(killed));
            assert_eq!(horizons.too_old(1), None, "the later snapshot holds nothing back");
            let error = killed.error().to_string();
            assert!(
                error.starts_with(
                    "TransactionContext Error: snapshot too old: it was 6000 commits behind and \
                     held "
                ),
                "{error}"
            );

            // With the killed snapshot left out every record is below the horizon. Its statement
            // ending, and the later snapshot's statements moving on, let the chunks go.
            horizons.end(0);
            for _ in 0..4 {
                horizons.end(1);
                horizons.resume(1, later);
                assert_eq!(space.collect(&horizons, &clock, &heads).killed, None);
            }
            assert_eq!(space.live_chunks(), 0);
            assert_eq!(space.spilled_bytes(), 0);
            for (row, &rid) in rids.iter().enumerate() {
                let last = old + 3 * (row as u64 + 1);
                assert_eq!(as_of(&items, rid, later), Some(u128::from(last)));
            }
            horizons.end(1);
        }

        #[test]
        fn writers_readers_and_the_collector_run_together() {
            together(None, 20_000, 0);
        }

        /// The same with every record the collector can spill spilled, and readers that stay at
        /// their snapshot for 8,000 commits. That is 2,000 records of each writer, more than the
        /// 1,024 of a chunk, so sealed chunks hold records above the horizon to spill.
        #[test]
        fn writers_readers_and_a_collector_that_spills_run_together() {
            let directory = spill_directory("together");
            let limits = SpillLimits { directory: directory.clone(), at: 0, most: 1 << 40 };
            together(Some(limits), 4_000, 8_000);
            assert_eq!(spill_files(&directory), Vec::<u64>::new(), "the file goes with the space");
        }

        /// Writers, readers and the collector at once. Every value a reader sees is checked
        /// against the commits afterwards: the newest commit to the row at or below its snapshot.
        /// Each reader reads every row at one snapshot over and over until `hold` commits have gone
        /// by, and once at the least. The writers start once every reader holds its first snapshot,
        /// so a reader holds one while the first chunks seal however the threads are scheduled.
        /// When the collector spills, a reader holds on and each writer waits before its last
        /// update until it has spilled something, or a minute went by, since the few milliseconds
        /// a chunk waits sealed under a held snapshot can pass without the collector being run.
        fn together(spill: Option<SpillLimits>, updates: u64, hold: u64) {
            const WRITERS: usize = 4;
            const READERS: usize = 2;
            let spills = spill.is_some();
            let space = Arc::new(UndoSpace::new());
            space.spill_at(spill);
            let items = table(&space, 1);
            let rids = rows(&items, WRITERS as u32);
            let clock = Arc::new(AtomicU64::new(1));
            let commit = Arc::new(Mutex::new(()));
            let horizons = Arc::new(Horizons::new(WRITERS + READERS));
            let heads = Arc::new(Tables(vec![Arc::clone(&items)]));
            let writing = Arc::new(AtomicU64::new(WRITERS as u64));
            let spilled = Arc::new(AtomicU64::new(0));
            let start = Arc::new(Barrier::new(WRITERS + READERS));
            let began = Instant::now();
            let waited = move |spilled: &AtomicU64| {
                !spills || spilled.load(Ordering::SeqCst) > 0 || began.elapsed().as_secs() >= 60
            };

            let writers: Vec<_> = (0..WRITERS)
                .map(|worker| {
                    let (items, clock, commit) =
                        (Arc::clone(&items), Arc::clone(&clock), Arc::clone(&commit));
                    let (horizons, space, writing) =
                        (Arc::clone(&horizons), Arc::clone(&space), Arc::clone(&writing));
                    let (start, spilled) = (Arc::clone(&start), Arc::clone(&spilled));
                    let rid = rids[worker];
                    thread::spawn(move || {
                        start.wait();
                        let Some(Stripe::Hot(stripe)) = items.find(rid.stripe()) else {
                            panic!("hot")
                        };
                        let mut buffer = UndoBuffer::default();
                        let mut commits = Vec::new();
                        for i in 0..updates {
                            while i + 1 == updates && !waited(&spilled) {
                                thread::yield_now();
                            }
                            let txn = (worker as u64) << 32 | (i + 2);
                            let snapshot = horizons.begin(worker, &clock, Reads::Tables(vec![1]));
                            stripe
                                .update(
                                    rid.slot(),
                                    &[(0, Some(txn.into()))],
                                    snapshot,
                                    txn,
                                    Wait::NEVER,
                                    &mut buffer,
                                )
                                .expect("each writer has a row of its own");
                            let guard = commit.lock().expect("the commit lock");
                            let ts = clock.load(Ordering::SeqCst) + 1;
                            stripe.commit_write(rid.slot(), txn, ts);
                            clock.store(ts, Ordering::SeqCst);
                            drop(guard);
                            horizons.end(worker);
                            commits.push((ts, txn));
                        }
                        buffer.seal(&space);
                        writing.fetch_sub(1, Ordering::SeqCst);
                        commits
                    })
                })
                .collect();
            let readers: Vec<_> = (0..READERS)
                .map(|reader| {
                    let (items, clock, horizons, writing) = (
                        Arc::clone(&items),
                        Arc::clone(&clock),
                        Arc::clone(&horizons),
                        Arc::clone(&writing),
                    );
                    let rids = rids.clone();
                    let (start, spilled) = (Arc::clone(&start), Arc::clone(&spilled));
                    thread::spawn(move || {
                        let mut seen = Vec::new();
                        let mut first = true;
                        while writing.load(Ordering::SeqCst) > 0 {
                            let worker = WRITERS + reader;
                            let snapshot = horizons.begin(worker, &clock, Reads::Tables(vec![1]));
                            if std::mem::take(&mut first) {
                                start.wait();
                            }
                            loop {
                                for (row, &rid) in rids.iter().enumerate() {
                                    seen.push((row, snapshot, as_of(&items, rid, snapshot)));
                                }
                                if (clock.load(Ordering::SeqCst) >= snapshot + hold
                                    && waited(&spilled))
                                    || writing.load(Ordering::SeqCst) == 0
                                {
                                    break;
                                }
                                thread::yield_now();
                            }
                            horizons.end(worker);
                        }
                        seen
                    })
                })
                .collect();
            let collector = {
                let (space, horizons, clock, heads, writing, spilled) = (
                    Arc::clone(&space),
                    Arc::clone(&horizons),
                    Arc::clone(&clock),
                    Arc::clone(&heads),
                    Arc::clone(&writing),
                    Arc::clone(&spilled),
                );
                thread::spawn(move || {
                    while writing.load(Ordering::SeqCst) > 0 {
                        let done = space.collect(&horizons, &clock, &*heads).spilled;
                        spilled.fetch_add(done as u64, Ordering::SeqCst);
                        thread::yield_now();
                    }
                })
            };

            let commits: Vec<Vec<(u64, u64)>> =
                writers.into_iter().map(|writer| writer.join().expect("writes")).collect();
            collector.join().expect("collects");
            let spilled = spilled.load(Ordering::SeqCst);
            assert_eq!(spills, spilled > 0, "{spilled} records spilled");
            let seen: Vec<_> =
                readers.into_iter().map(|reader| reader.join().expect("reads")).collect();
            // The collector's own chunk, sealed once nothing else waits, goes a round after.
            for _ in 0..4 {
                space.collect(&horizons, &clock, &*heads);
            }
            assert_eq!(space.live_chunks(), 0, "every chunk was sealed and nobody is reading");
            assert_eq!(space.spilled_bytes(), 0, "and no stub points into the file");
            for seen in seen {
                for (row, snapshot, value) in seen {
                    let expected = commits[row]
                        .iter()
                        .take_while(|&&(ts, _)| ts <= snapshot)
                        .last()
                        .map_or(0, |&(_, txn)| u128::from(txn));
                    assert_eq!(value, Some(expected), "row {row} at {snapshot}");
                }
            }
        }
    }
}
