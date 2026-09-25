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
//! `ts` is the only thing written after, with its own atomic store. The words are relaxed atomics
//! for the same reason the hot stripe's cells are: a reader that holds a reference it read in a
//! race still reads defined values.

use std::sync::OnceLock;
use std::sync::atomic::{AtomicU32, AtomicU64, Ordering};

/// Words in a chunk: 64 KiB.
const CHUNK_WORDS: usize = 8_192;

/// Chunks in a directory, and directories in a space, for 2^20 chunks.
const DIRECTORY: usize = 1_024;

/// Words of a record's header: prev, kind and image count; ts; rid.
const HEADER_WORDS: usize = 3;

/// Words of one column image: the column and its validity, then the 16-byte value.
const IMAGE_WORDS: usize = 3;

/// The most images one record holds. A wider update writes several records.
pub const MOST_IMAGES: usize = (CHUNK_WORDS - HEADER_WORDS) / IMAGE_WORDS;

/// The reference of an undo record, or of none when it is 0.
pub type UndoRef = u32;

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
        }
    }

    /// Chunks made so far.
    #[must_use]
    pub fn chunks(&self) -> u32 {
        self.chunks.load(Ordering::Relaxed).min(1 << 20) - 1
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

    /// A new chunk's id, or `None` when all 2^20 are made.
    fn make(&self) -> Option<u32> {
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

impl UndoBuffer {
    /// Writes a record of `kind` for row `rid` stamped `ts`, after `prev`, and returns its
    /// reference, not yet published. An update of more than [`MOST_IMAGES`] columns is written as
    /// a chain of records, and the reference is the newest.
    ///
    /// `None` once the space has made all its chunks.
    pub fn write(
        &mut self,
        space: &UndoSpace,
        kind: UndoKind,
        ts: u64,
        rid: u64,
        mut prev: UndoRef,
        images: &[Image],
    ) -> Option<UndoRef> {
        let mut pieces = images.chunks(MOST_IMAGES);
        let first = pieces.next().unwrap_or_default();
        for piece in std::iter::once(first).chain(pieces) {
            prev = self.one(space, kind, ts, rid, prev, piece)?;
        }
        Some(prev)
    }

    fn one(
        &mut self,
        space: &UndoSpace,
        kind: UndoKind,
        ts: u64,
        rid: u64,
        prev: UndoRef,
        images: &[Image],
    ) -> Option<UndoRef> {
        let len = HEADER_WORDS + IMAGE_WORDS * images.len();
        let units = len.div_ceil(2) as u32;
        if self.chunk == 0 || self.next + units > (CHUNK_WORDS / 2) as u32 {
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
        record[2].store(rid, Ordering::Relaxed);
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

    use super::{Image, MOST_IMAGES, UndoBuffer, UndoKind, UndoSpace};

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
        let first = buffer.write(&space, UndoKind::Update, 7 | 1 << 63, 42, 0, &images(4, 9));
        let first = first.expect("room");
        let second = buffer.write(&space, UndoKind::Delete, 8 | 1 << 63, 42, first, &[]);
        let second = second.expect("room");
        let record = space.record(second).expect("written");
        assert_eq!(record.kind(), UndoKind::Delete);
        assert_eq!(record.prev(), first);
        assert_eq!(record.rid(), 42);
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
            refs.push(buffer.write(&space, UndoKind::Update, i, i, 0, &images(5, i.into())));
        }
        assert!(space.chunks() > 1, "3,000 records of 144 bytes are more than 64 KiB");
        for (i, at) in refs.into_iter().enumerate() {
            let record = space.record(at.expect("room")).expect("written");
            assert_eq!(record.ts(), i as u64);
            assert_eq!(record.images().collect::<Vec<_>>(), images(5, i as u128));
        }
        let wide = images(MOST_IMAGES * 2 + 10, 3);
        let mut at = buffer.write(&space, UndoKind::Update, 1, 1, 0, &wide).expect("room");
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
                            buffer.write(&space, UndoKind::Update, worker, i, 0, &image)
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
}
