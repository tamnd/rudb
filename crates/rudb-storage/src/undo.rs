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

use std::collections::HashMap;
use std::sync::atomic::{AtomicU32, AtomicU64, Ordering};
use std::sync::{Mutex, OnceLock, PoisonError};

use crate::deletes::UNCOMMITTED;
use crate::horizon::Horizons;

/// Words in a chunk: 64 KiB.
const CHUNK_WORDS: usize = 8_192;

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
}

/// Where the collector finds a record's chain, which is the hot stripe the rid names in the
/// table.
pub trait Heads {
    /// Takes the record at `record` out of the chain of `rid` in `table`, if the row's stripe is
    /// still hot and the record is still in its chain, and every record older than it as well
    /// when `older` says so.
    fn unlink(&self, table: u32, rid: u64, record: UndoRef, older: bool);
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
        }
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
        rounds.waiting.append(&mut self.sealed.lock().unwrap_or_else(PoisonError::into_inner));

        let (ready, cut): (Vec<_>, Vec<_>) =
            rounds.cut.drain(..).partition(|held| horizons.passed(held.epoch, &held.tables));
        rounds.cut = cut;
        done.freed = ready.len();
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
        rounds.waiting.retain(|&(chunk, units)| {
            let mut tables = Vec::new();
            let below = self.records(chunk, units).all(|(_, record)| {
                let (ts, table) = (record.ts(), record.table());
                if !tables.contains(&table) {
                    tables.push(table);
                }
                ts == ABORTED
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

        // After the cuts and the reads of every stamp above, so a statement that begins at this
        // epoch or later cannot reach a record cut here, and one that saw a value an abort took
        // back began before it.
        let epoch = horizons.advance();
        rounds.cut.extend(cut.into_iter().map(|held| Held { epoch, ..held }));
        rounds.found.extend(found.into_iter().map(|held| Held { epoch, ..held }));
        done
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

/// One undo record, read in place.
#[derive(Debug, Clone, Copy)]
pub struct Record<'a> {
    words: &'a [AtomicU64],
}

impl<'a> Record<'a> {
    /// The next older record of the same row.
    #[must_use]
    pub fn prev(&self) -> UndoRef {
        self.words[0].load(Ordering::Relaxed) as u32
    }

    /// Points it at `prev` instead, which is how the collector takes the record after it out of
    /// the chain, and how a writer that lost a race for the row's head moves a record it has not
    /// published yet onto the new one.
    pub(crate) fn set_prev(&self, prev: UndoRef) {
        let _ = self.words[0].fetch_update(Ordering::Relaxed, Ordering::Relaxed, |word| {
            Some((word & !u64::from(u32::MAX)) | u64::from(prev))
        });
    }

    /// What it undoes.
    #[must_use]
    pub fn kind(&self) -> UndoKind {
        if (self.words[0].load(Ordering::Relaxed) >> 32) as u8 == 1 {
            UndoKind::Delete
        } else {
            UndoKind::Update
        }
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
        let kind = u64::from(kind == UndoKind::Delete);
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
        use std::sync::atomic::{AtomicU64, Ordering};
        use std::sync::{Arc, Mutex};
        use std::thread;

        use crate::horizon::{Horizons, Reads};
        use crate::hot::Width;
        use crate::park::Wait;
        use crate::stripes::{Inserter, Rid, Stripe, StripeDirectory};
        use crate::undo::{Heads, UndoBuffer, UndoRef, UndoSpace};

        /// Several tables, as the database finds a record's chain.
        struct Tables(Vec<Arc<StripeDirectory>>);

        impl Heads for Tables {
            fn unlink(&self, table: u32, rid: u64, record: UndoRef, older: bool) {
                for directory in &self.0 {
                    directory.unlink(table, rid, record, older);
                }
            }
        }

        fn table(space: &Arc<UndoSpace>, id: u32) -> Arc<StripeDirectory> {
            Arc::new(StripeDirectory::new(&[Width::Eight], Arc::clone(space)).in_table(id))
        }

        /// Rows of value 0 in `table`, committed by transaction 1 at timestamp 1.
        fn rows(table: &Arc<StripeDirectory>, n: u32) -> Vec<Rid> {
            let mut inserter = Inserter::new(Arc::clone(table));
            let (lease, slots) = inserter.take(n);
            for slot in slots.clone() {
                lease.stripe().write(slot, 0, Some(0));
            }
            lease.stripe().insert(slots.clone(), 1);
            lease.stripe().commit_insert(slots.clone(), 1, 1);
            inserter.rids(slots)
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
            let horizons = Horizons::new(1);
            let heads = Tables(vec![Arc::clone(&orders), Arc::clone(&stock)]);
            let old = horizons.begin(0, &clock, Reads::Tables(vec![1]));
            let (mut first, mut second) = (UndoBuffer::default(), UndoBuffer::default());
            updates(&orders, order, &clock, 3_000, &mut first);
            updates(&stock, item, &clock, 3_000, &mut second);
            first.seal(&space);
            second.seal(&space);
            let made = space.chunks() as usize;

            for _ in 0..3 {
                space.collect(&horizons, &clock, &heads);
            }
            let kept = space.live_chunks();
            assert!(kept < made && kept >= made / 2 - 1, "{kept} of {made}");
            assert_eq!(as_of(&orders, order, old), Some(0), "the old snapshot still reads");
            assert_eq!(as_of(&orders, order, old + 10), Some(u128::from(old + 10)));

            horizons.end(0);
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

        /// Writers, readers and the collector at once. Every value a reader sees is checked
        /// against the commits afterwards: the newest commit to the row at or below its snapshot.
        #[test]
        fn writers_readers_and_the_collector_run_together() {
            const WRITERS: usize = 4;
            const READERS: usize = 2;
            const UPDATES: u64 = 20_000;
            let space = Arc::new(UndoSpace::new());
            let items = table(&space, 1);
            let rids = rows(&items, WRITERS as u32);
            let clock = Arc::new(AtomicU64::new(1));
            let commit = Arc::new(Mutex::new(()));
            let horizons = Arc::new(Horizons::new(WRITERS + READERS));
            let heads = Arc::new(Tables(vec![Arc::clone(&items)]));
            let writing = Arc::new(AtomicU64::new(WRITERS as u64));

            let writers: Vec<_> = (0..WRITERS)
                .map(|worker| {
                    let (items, clock, commit) =
                        (Arc::clone(&items), Arc::clone(&clock), Arc::clone(&commit));
                    let (horizons, space, writing) =
                        (Arc::clone(&horizons), Arc::clone(&space), Arc::clone(&writing));
                    let rid = rids[worker];
                    thread::spawn(move || {
                        let Some(Stripe::Hot(stripe)) = items.find(rid.stripe()) else {
                            panic!("hot")
                        };
                        let mut buffer = UndoBuffer::default();
                        let mut commits = Vec::new();
                        for i in 0..UPDATES {
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
                    thread::spawn(move || {
                        let mut seen = Vec::new();
                        while writing.load(Ordering::SeqCst) > 0 {
                            let worker = WRITERS + reader;
                            let snapshot = horizons.begin(worker, &clock, Reads::Tables(vec![1]));
                            for (row, &rid) in rids.iter().enumerate() {
                                seen.push((row, snapshot, as_of(&items, rid, snapshot)));
                            }
                            horizons.end(worker);
                        }
                        seen
                    })
                })
                .collect();
            let collector = {
                let (space, horizons, clock, heads, writing) = (
                    Arc::clone(&space),
                    Arc::clone(&horizons),
                    Arc::clone(&clock),
                    Arc::clone(&heads),
                    Arc::clone(&writing),
                );
                thread::spawn(move || {
                    while writing.load(Ordering::SeqCst) > 0 {
                        space.collect(&horizons, &clock, &*heads);
                        thread::yield_now();
                    }
                })
            };

            let commits: Vec<Vec<(u64, u64)>> =
                writers.into_iter().map(|writer| writer.join().expect("writes")).collect();
            collector.join().expect("collects");
            let seen: Vec<_> =
                readers.into_iter().map(|reader| reader.join().expect("reads")).collect();
            for _ in 0..3 {
                space.collect(&horizons, &clock, &*heads);
            }
            assert_eq!(space.live_chunks(), 0, "every chunk was sealed and nobody is reading");
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
