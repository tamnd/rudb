//! The strings arena of a hot stripe, `engine-v4/07-the-head.md` section 7.3.
//!
//! A text value longer than 12 bytes lives here and its 16-byte view points at it by chunk and
//! offset. The arena is append-only: bytes are written once, by the worker that carved them out,
//! and never again, so a view in an undo record keeps pointing at valid bytes until the stripe
//! freezes and the arena goes with it.
//!
//! Chunks are 1 MiB. Workers do not share a cursor per string. Each takes a 64 KiB sub-chunk with
//! one compare-and-swap on the arena's cursor and fills it alone, and a string longer than a
//! sub-chunk gets a chunk of its own, sized to fit.
//!
//! The bytes are held in relaxed `AtomicU64` words and each string starts on a word. A view is two
//! words, and a scan that races an in-place update can read the length of one view and the
//! reference of another. With plain bytes, such a read could reach a range another worker is
//! writing at that moment, which is undefined behaviour. With relaxed atomics it reads some bytes,
//! bounded by the chunk, and the undo chain is what tells the scan to read the row again.

use std::sync::OnceLock;
use std::sync::atomic::{AtomicU32, AtomicU64, Ordering};

/// Bytes in a shared chunk.
pub const CHUNK_BYTES: u32 = 1 << 20;

/// Bytes a worker carves out of a shared chunk at a time.
pub const CARVE_BYTES: u32 = 64 << 10;

/// Chunks in a directory, and directories in an arena, for 65,536 chunks at most.
const DIRECTORY: usize = 256;

/// The cursor before the first chunk is made.
const NO_CHUNK: u64 = u64::MAX;

/// Where a string was put: its chunk and its byte offset in the chunk.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Place {
    /// The chunk.
    pub chunk: u32,
    /// The byte offset, a multiple of 8.
    pub offset: u32,
}

/// A worker's carved range: bytes `next..end` of `chunk` are its own.
#[derive(Debug, Clone, Default)]
pub struct Space {
    chunk: u32,
    next: u32,
    end: u32,
}

impl Space {
    /// Bytes left in the range.
    #[must_use]
    pub fn left(&self) -> u32 {
        self.end - self.next
    }
}

type Chunk = Box<[AtomicU64]>;

/// 256 chunk ids, each set once.
type Directory = Box<[OnceLock<Chunk>]>;

/// Append-only string bytes shared by the workers of one stripe.
#[derive(Debug)]
pub struct Arena {
    directories: Box<[OnceLock<Directory>]>,
    /// Chunk ids handed out.
    chunks: AtomicU32,
    /// The shared chunk being carved and the offset of its next sub-chunk, `chunk << 32 | offset`.
    cursor: AtomicU64,
    /// Bytes of every chunk made.
    bytes: AtomicU64,
}

impl Default for Arena {
    fn default() -> Self {
        Self::new()
    }
}

impl Arena {
    /// An arena with no chunks.
    #[must_use]
    pub fn new() -> Self {
        Self {
            directories: (0..DIRECTORY).map(|_| OnceLock::new()).collect(),
            chunks: AtomicU32::new(0),
            cursor: AtomicU64::new(NO_CHUNK),
            bytes: AtomicU64::new(0),
        }
    }

    /// Copies `bytes` into the arena, from `space` when it has room and from a fresh carve when it
    /// does not. `None` once the arena has made all 65,536 of its chunks, or for a string longer
    /// than a view can say.
    pub fn put(&self, space: &mut Space, bytes: &[u8]) -> Option<Place> {
        let len = u32::try_from(bytes.len()).ok()?;
        let rounded = len.checked_next_multiple_of(8)?;
        let place = if len > CARVE_BYTES {
            Place { chunk: self.make(rounded)?, offset: 0 }
        } else {
            if space.left() < rounded {
                *space = self.carve()?;
            }
            let place = Place { chunk: space.chunk, offset: space.next };
            space.next += rounded;
            place
        };
        let words = &self.chunk(place.chunk)?[place.offset as usize / 8..];
        for (word, piece) in words.iter().zip(bytes.chunks(8)) {
            let mut padded = [0_u8; 8];
            padded[..piece.len()].copy_from_slice(piece);
            word.store(u64::from_le_bytes(padded), Ordering::Relaxed);
        }
        Some(place)
    }

    /// Appends the `len` bytes at `place` to `out`, or as many of them as the chunk holds, since a
    /// torn read of a view can ask for more than was ever put there.
    pub fn read(&self, place: Place, len: u32, out: &mut Vec<u8>) {
        let Some(words) = self.chunk(place.chunk) else { return };
        let words = words.get(place.offset as usize / 8..).unwrap_or_default();
        let start = out.len();
        let len = (len as usize).min(words.len() * 8);
        for word in &words[..len.div_ceil(8)] {
            out.extend_from_slice(&word.load(Ordering::Relaxed).to_le_bytes());
        }
        out.truncate(start + len);
    }

    /// Bytes of every chunk made.
    #[must_use]
    pub fn bytes(&self) -> u64 {
        self.bytes.load(Ordering::Relaxed)
    }

    /// A sub-chunk of the shared chunk, or a whole fresh chunk when the shared one is used up.
    fn carve(&self) -> Option<Space> {
        let mut cursor = self.cursor.load(Ordering::Acquire);
        loop {
            let (chunk, offset) = ((cursor >> 32) as u32, cursor as u32);
            if cursor != NO_CHUNK && offset < CHUNK_BYTES {
                let next = cursor + u64::from(CARVE_BYTES);
                match self.cursor.compare_exchange_weak(
                    cursor,
                    next,
                    Ordering::AcqRel,
                    Ordering::Acquire,
                ) {
                    Ok(_) => return Some(Space { chunk, next: offset, end: offset + CARVE_BYTES }),
                    Err(now) => cursor = now,
                }
                continue;
            }
            let fresh = self.make(CHUNK_BYTES)?;
            let next = (u64::from(fresh) << 32) | u64::from(CARVE_BYTES);
            // Whether or not the fresh chunk becomes the shared one, the carve it starts with is
            // this worker's. When another worker installed a chunk first, this one keeps all of
            // the fresh chunk, which is not shared with anyone.
            let end = match self.cursor.compare_exchange(
                cursor,
                next,
                Ordering::AcqRel,
                Ordering::Acquire,
            ) {
                Ok(_) => CARVE_BYTES,
                Err(_) => CHUNK_BYTES,
            };
            return Some(Space { chunk: fresh, next: 0, end });
        }
    }

    /// A new chunk of `bytes`, a multiple of 8, and its id.
    fn make(&self, bytes: u32) -> Option<u32> {
        let id = self.chunks.fetch_add(1, Ordering::Relaxed);
        let directory = self.directories.get(id as usize / DIRECTORY)?;
        let directory = directory.get_or_init(|| (0..DIRECTORY).map(|_| OnceLock::new()).collect());
        let words = (0..bytes / 8).map(|_| AtomicU64::new(0)).collect();
        // The id came from a `fetch_add`, so nobody else sets this entry.
        let _ = directory[id as usize % DIRECTORY].set(words);
        self.bytes.fetch_add(u64::from(bytes), Ordering::Relaxed);
        Some(id)
    }

    fn chunk(&self, id: u32) -> Option<&[AtomicU64]> {
        let directory = self.directories.get(id as usize / DIRECTORY)?.get()?;
        directory[id as usize % DIRECTORY].get().map(|chunk| &chunk[..])
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;
    use std::thread;

    use super::{Arena, CARVE_BYTES, CHUNK_BYTES, Place, Space};

    fn read(arena: &Arena, place: Place, len: usize) -> Vec<u8> {
        let mut out = Vec::new();
        arena.read(place, u32::try_from(len).expect("short"), &mut out);
        out
    }

    #[test]
    fn strings_come_back_as_they_went_in() {
        let arena = Arena::new();
        let mut space = Space::default();
        let strings: Vec<Vec<u8>> =
            (0..5_000_u32).map(|i| (0..13 + i % 300).map(|j| (i ^ j) as u8).collect()).collect();
        let places: Vec<Place> =
            strings.iter().map(|s| arena.put(&mut space, s).expect("room")).collect();
        for (string, place) in strings.iter().zip(&places) {
            assert_eq!(place.offset % 8, 0);
            assert_eq!(&read(&arena, *place, string.len()), string);
        }
        assert_eq!(arena.bytes(), u64::from(CHUNK_BYTES), "under 1 MiB of strings");
    }

    #[test]
    fn a_long_string_gets_a_chunk_of_its_own() {
        let arena = Arena::new();
        let mut space = Space::default();
        let short = arena.put(&mut space, b"a string of twenty bytes").expect("room");
        let long: Vec<u8> = (0..CARVE_BYTES * 3 + 5).map(|i| i as u8).collect();
        let place = arena.put(&mut space, &long).expect("room");
        assert_eq!(place.offset, 0);
        assert_ne!(place.chunk, short.chunk);
        assert_eq!(read(&arena, place, long.len()), long);
        assert_eq!(arena.bytes(), u64::from(CHUNK_BYTES) + u64::from(CARVE_BYTES * 3 + 8));
        let after = arena.put(&mut space, b"and one more after it").expect("room");
        assert_eq!(after.chunk, short.chunk, "the worker's carve is still its own");
    }

    #[test]
    fn a_read_past_what_was_put_stops_at_the_chunk() {
        let arena = Arena::new();
        let mut space = Space::default();
        let place = arena.put(&mut space, b"thirteen byte").expect("room");
        assert_eq!(read(&arena, place, 100).len(), 100, "the zeros after it");
        let end = Place { chunk: place.chunk, offset: CHUNK_BYTES - 8 };
        assert_eq!(read(&arena, end, 100).len(), 8);
        assert!(read(&arena, Place { chunk: 9, offset: 0 }, 10).is_empty(), "no such chunk");
        let past = Place { chunk: place.chunk, offset: u32::MAX - 7 };
        assert!(read(&arena, past, 10).is_empty());
    }

    #[test]
    fn workers_carve_without_overlap() {
        let arena = Arc::new(Arena::new());
        let workers: Vec<_> = (0..8_u8)
            .map(|worker| {
                let arena = Arc::clone(&arena);
                thread::spawn(move || {
                    let mut space = Space::default();
                    (0..20_000_u32)
                        .map(|i| {
                            let string = vec![worker; 13 + (i % 50) as usize];
                            (arena.put(&mut space, &string).expect("room"), string)
                        })
                        .collect::<Vec<_>>()
                })
            })
            .collect();
        for handle in workers {
            for (place, string) in handle.join().expect("the worker finishes") {
                assert_eq!(read(&arena, place, string.len()), string);
            }
        }
    }
}
