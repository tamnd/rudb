//! Rudb's single-file columnar snapshot format.
//!
//! A committed directory names independently readable column pages. It has two levels: a catalog
//! directory naming every table in the file, which is what a footer slot points at and what opening
//! a database reads, and one directory per table under it holding that table's stripes, pages and
//! statistics. One slot write publishes all of them, so a commit is atomic across tables.
//!
//! This version handles scalar columns; the file header has two generation slots so an unfinished
//! replacement directory cannot hide the last complete one. See
//! `spec/storage-v3/12-many-tables-in-one-file.md`.
//!
//! # Parts and stripes
//!
//! A part is one appended chunk, which is a thousand rows, and it is the unit a scan decodes and
//! hands to the pipeline. A stripe is sixty four parts, and it is the unit the directory describes
//! and the unit the file is laid out in: one page per column per stripe, holding that column's
//! sixty four part payloads end to end.
//!
//! The two are separate because they are sized by different pressures. A part wants to be small
//! because it is a vector and vectors live in cache. A stripe wants to be large because everything
//! the directory holds is per stripe and the directory is one buffer that has to be read and
//! decoded before a single row can be answered. A hundred million rows of the hundred and five
//! column ClickBench table is ninety seven thousand parts, and a directory with a page entry and a
//! pair of bounds per part per column is several hundred megabytes, which is what made that load
//! fail before this split existed. Sixty four parts to a stripe divides that by sixty four.
//!
//! Where the parts of a page start is not in the directory either, for the same reason. Each
//! stripe writes one index page holding a length and a checksum per part per column, and a reader
//! preads the sixty four entries belonging to the column it wants. A scan reads the whole column
//! page once and slices it; a sparse row fetch reads the index entries and then only the part it
//! needs.

#![forbid(unsafe_code)]

use std::borrow::Cow;
use std::cmp::{Ordering, Reverse};
use std::collections::{BTreeMap, HashMap, VecDeque};
use std::fs::File;
use std::mem::{size_of, size_of_val};
use std::path::Path;
use std::slice;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering as Atomic};
use std::sync::{Arc, Condvar, Mutex, OnceLock, PoisonError, Weak};

use rudb_common::bounds::{self, Bound, Op, scaled_as};
use rudb_common::{Clustering, Error, Field, LogicalType, PhysicalType, Result, Value, Width};
use rudb_encoding::{bitpack, chooser, integer, string};
use rudb_io::{Filesystem, OpenMode, RealFilesystem};
use rudb_metrics::{LoadProfile, Stage};
use rudb_storage::sieve::Sieve;
use rudb_storage::{Probe, Range, Zone};
use rudb_vector::string::StringColumn;
use rudb_vector::validity::Validity;
use rudb_vector::{Buffer, Chunk, Data, Packed, TextSource, Vector, search_below};

mod distinct;
pub mod graph;
pub mod host;
mod prepare;
mod projection;
mod run_projection;
use prepare::Lent;
pub mod section;
pub mod stats;
mod zones;

pub use prepare::{Building, DICTIONARY_CAP_BYTES, Merged, Merger, Paged, Prepared, Preparer};
pub use projection::build_sorted_projection;
pub use run_projection::build_run_projection;
pub use section::Section;
pub use zones::{Common, Stripes, ascending, distincts};

const MAGIC: &[u8; 8] = b"RUDBNV10";
const DIRECTORY: &[u8; 8] = b"RUDBDI10";
const CATALOG: &[u8; 8] = b"RUDBCA10";
const NONZERO_COUNTS: &[u8; 8] = b"RUDBNZ10";
const AGGREGATE_SUMS: &[u8; 8] = b"RUDBAG10";
const DISTINCT_COUNTS: &[u8; 8] = b"RUDBDC10";
const INTEGER_EXTREMES: &[u8; 8] = b"RUDBEX10";
const COMPLETE_FREQUENCIES: &[u8; 8] = b"RUDBFQ10";
const MAX_CATALOG_FREQUENCIES: usize = 64;
const FORMAT: u32 = 29;

/// Formats this build can open.
///
/// More than one, for the first time, and the reason is spec/graph/10-milestones.md's G1 exit
/// criterion: a build with the section table in it has to open a file written before the section
/// table existed, unchanged and without a rewrite. Formats 22 and 23 are those files, and both read
/// as a table with an empty section table, which is exactly what section 3.1 says a table with no
/// graph sections is.
///
/// All three of the older ones are readable for the same reason. What took the format from 22 to 23
/// was tags for fourteen more column types, and a file written before that has none of them in it,
/// so nothing in an older file is a tag this build cannot read. What took it from 23 to 24 is the
/// section table, which a file written before it simply does not have. What takes it from 24 to 25
/// is the view section on the end of the catalog, which an older file does not have either, and a
/// catalog that ends where the tables end reads as a catalog with no views in it. What takes it
/// from 25 to 26 is that a global dictionary's payload blocks now say where they are, and a file
/// written before that has them behind one another, which [`open_global_dictionary`] reads by
/// turning the ends it finds into the same places the newer files name outright. What takes it
/// from 26 to 27 is that those blocks are written into the file as the load goes, between the
/// stripes, rather than behind the dictionary's index at the end, so the dictionary's page is the
/// index and the sorted order and nothing else. A format 26 file has its blocks inside the page,
/// and the reader tells the two apart by whether the page has room left over for them.
///
/// Format 28 adds per-payload-block substring signatures to global string dictionaries. Older
/// files have no signatures and use the ordinary exact string filter. Format 29 makes each
/// signature four times as wide, which a dictionary says with [`DICTIONARY_WIDE_GRAMS`], and a
/// format 28 file is read with the narrow ones it has.
///
/// This is not a general compatibility promise. Seven formats are readable because there was a
/// specific reason for each, and the list shrinks again the moment the older ones stop being worth
/// carrying.
const READABLE: &[u32] = &[22, 23, 24, 25, 26, 27, 28, FORMAT];

const HEADER: u64 = 80;
const SLOT_BYTES: usize = 28;
const MAX_PAGE: usize = 256 * 1024 * 1024;
const MAX_DIRECTORY: usize = 128 * 1024 * 1024;
const FREQUENCIES_V2: &[u8; 8] = b"RUDBFQ2\0";
const FREQUENCIES: &[u8; 8] = b"RUDBFQ3\0";
/// Inline spellings for string entries in the bounded frequency synopsis.
///
/// A planner usually asks about one literal such as the empty string. Without this block it opens
/// a multi-million-value global dictionary and visits the payload blocks of every retained entry
/// merely to compare that literal with at most 512 heavy hitters. The spellings are already in
/// memory while the writer sorts the dictionary, so storing this bounded copy makes planning a
/// directory read and leaves the dictionary unopened.
const FREQUENCY_TEXTS: &[u8; 8] = b"RUDBFT1\0";
/// Certified host aggregate state for the version-one anchored replacement expression.
const HOST_GROUPS: &[u8; 8] = b"RUDBHG1\0";
/// Exact leading counts for a bounded pair of dictionary-backed grouping keys.
///
/// This is a separate optional directory block rather than another frequency format. Readers that
/// predate it still understand every earlier directory, and a table without a pair worth keeping
/// writes no block at all.
const PAIR_FREQUENCIES: &[u8; 8] = b"RUDBPF1\0";
/// The clustering declaration, written after the frequencies and only when there is one.
///
/// No format bump for this, which is the convention the frequency section set in #728: a new
/// optional trailing section with its own magic leaves every file that does not use it byte for
/// byte what it was, and the version is bumped for a change to a layout that already exists, as
/// #1029 did. A file with no declaration is the same bytes this build wrote yesterday.
///
/// The width byte in this block gained a fifth value for #1285, for a declaration that leaves the
/// bucket to the row count, and that did not bump the format either. It is the one case where the
/// reasoning needs saying out loud, because it is a new value in a layout that already exists
/// rather than a new section. A build without it reading one of these says `clustering width
/// tag differs` and refuses the table, which is what that message was written for. Bumping the
/// format instead would have made every file this build writes unreadable to an older one, whether
/// it has a declaration in it or not, to warn about a case that only arises when it does.
const CLUSTERING: &[u8; 8] = b"RUDBCL1\0";
/// The string columns whose global dictionary stopped taking values partway through the load.
///
/// Section 5.5 of the encoding spec: a column whose stripes are nearly all new values, or the
/// fastest growing one once the dictionaries together pass their cap, stops adding to its
/// dictionary, and every stripe after that is written plainly. The stripes before keep their codes,
/// so the dictionary is still written and still decodes them, but it no longer holds every value of
/// the column, and nothing that reads it as if it did can be trusted: not the distinct count, not
/// the frequencies, not the sorted order's first and last value, and not the codes as a group key
/// or a membership index. A reader that finds a column named here decodes its coded pages to plain
/// strings and answers everything else the way it answers a column with no dictionary.
///
/// Same convention as [`CLUSTERING`], written only when a column was demoted, so a file with none
/// is the bytes it always was. A build that predates it refuses a file that has one with
/// `directory extension magic differs`, which is the right answer, because that build would trust
/// the dictionary.
///
/// A stripe written after the demotion has no membership index for the column. Its slot in the
/// stripe is written as a page of no bytes, which no real membership index is, since the smallest
/// one holds its code count.
const DEMOTED: &[u8; 8] = b"RUDBDM1\0";
/// The graph section table, written after the clustering declaration and written even when empty.
///
/// Same convention and the same reason as the block above it, with one difference: this one is
/// always there, so a file written by this build says which sections it has rather than leaving a
/// reader to infer it from where the bytes ran out. Section 3.1 of the graph spec is what makes
/// that safe to add without a format bump, because a table with no sections answers every query
/// the way it did before, only without the graph path.
const SECTIONS: &[u8; 8] = b"RUDBSE1\0";
/// How many bytes of each column's global dictionary live outside its page, written only when any do.
///
/// From format 27 a dictionary's payload blocks are written into the file while the load runs, so
/// they sit between the stripes and the dictionary's page covers only its index and sorted order.
/// Nothing needs the total to read the file, because the index names every block. It is here for
/// what a file costs a column, which [`Reader::layout`] and the statistics budget both report, and
/// which would otherwise lose most of the bytes of every large string column.
const DICTIONARY_PAYLOADS: &[u8; 8] = b"RUDBDP1\0";

/// The most sections one table's directory may name.
///
/// A relationship contributes at most three sections, so this bounds a table at a few thousand
/// relationships, which is far past anything a schema has. The bound is here so that a torn
/// directory naming four billion of them is refused at decode rather than turned into an
/// allocation, the same reason the extent count has one.
const MAX_SECTIONS: usize = 4096;
const FREQUENCY_CANDIDATES: usize = 32_768;
const FREQUENCY_ENTRIES: usize = 512;
const FREQUENCY_BUILD_RANK: usize = 10;
const FREQUENCY_ORDINALS: usize = 131_072;
const MAX_PAIR_FREQUENCIES: usize = 1024;
/// The most exact heavy-hitter text one column may copy into the directory.
///
/// A column with unusually large leading values keeps the old code-only synopsis instead. The
/// optimization must never turn a valid load into a directory-size failure.
const FREQUENCY_TEXT_BUDGET: usize = 1024 * 1024;
/// The most threads the two per column passes at the end of a commit are spread over.
///
/// A table like `hits` has ninety numeric columns, so on a machine with more cores than this the
/// cap is what decides how long the frequencies take rather than the columns are. It is here at all
/// because each worker holds a candidate table and a decoded part, and a hundred of those at once
/// on a narrow machine would be worse than waiting.
const MAX_FREQUENCY_WORKERS: usize = 32;

/// How many threads the passes at the end of a commit are spread over on this machine.
fn close_workers() -> usize {
    std::thread::available_parallelism().map_or(1, usize::from).min(MAX_FREQUENCY_WORKERS)
}

/// How many bytes the columns closing at the same time may hold between them.
///
/// Closing a global dictionary decodes every value it holds, sorts them and drops them, and #1356
/// took the columns one at a time so that five of them decoded at once were not the peak of a load.
/// A numeric column's frequencies hold a candidate table and, past it, an exact set of its distinct
/// values that reaches 512 MiB. The two used to run side by side with only the dictionaries under a
/// bound, and on the ClickBench `hits` 10M load the close took a load that had held 3.1 GB to 4.8
/// GB. A column is taken while the ones already closing leave room for it under this, and always
/// when nothing else is closing, so every dictionary of `hits` at 10M rows closes at once and `URL`
/// at 100M, which is past this alone, still closes on its own.
const CLOSE_BYTES: usize = 1 << 30;

/// What a numeric column's frequencies hold before its exact distinct set, which is the candidate
/// table, its recount and the page being read, with room to spare.
const NUMERIC_CLOSE_BYTES: usize = 4 << 20;

/// The most threads one stripe's encode is spread over.
///
/// Higher than the frequency cap because this is the load itself rather than a pass at the end of
/// it, and the work is one column of sixty four parts, which is large enough that a thread that
/// takes one is not a thread that was started for nothing. A machine with more cores than this has
/// the rest of them on the Parquet read, which is still one thread and is the other half of #808.
const MAX_ENCODE_WORKERS: usize = 32;

/// How much a writer appends before it asks the kernel to start writing it to the device.
///
/// Without it every byte of a load waits in the page cache for the sync at the commit, and that
/// sync was 1.3 to 1.7 s of a ClickBench `hits` 10M load of 8 to 9 s on the 32 core box. With it
/// the device writes while the load is still encoding. Thirty two megabytes is a few stripes of
/// `hits`, big enough that the call costs nothing next to the write, and small enough that what
/// is left for the commit is one stretch.
const WRITEBACK_STRETCH: u64 = 32 << 20;

/// The most bytes one column of one part may spend on a membership sieve.
///
/// A part is a thousand rows, so a filter sized for every one of them being distinct is about
/// thirteen hundred bytes and this never binds in practice. It is here so that a part that somehow
/// arrives much wider than a vector cannot put an unbounded index in the file. What does bind is the
/// rule in `Writer::encode_pages` that a sieve may not be as large as the part it indexes, which is a cap
/// per column rather than one number for the whole file.
const SIEVE_BUDGET: usize = 8 * 1024;

/// The most bytes one end of a per part range may spend on a string.
///
/// A bound is allowed to be wider than the truth and never narrower, so a long string is cut down to
/// this many bytes for the low end and cut down and then stepped up for the high end. The reason for
/// a cap at all is that there are nine hundred and seventy four parts of a hundred and five columns
/// in a million rows of ClickBench and `URL` runs to hundreds of bytes, so keeping every end whole
/// would put more in the directory than the skipping is worth. Twenty four bytes is past the point
/// where two URLs of the same site still look alike.
const PART_BOUND_BYTES: usize = 24;

fn io(error: std::io::Error) -> Error {
    Error::io(error.to_string())
}

fn invalid(message: &str) -> Error {
    Error::invalid_input(format!("invalid rudb native file: {message}"))
}

/// Adds a sequence of byte counts without an overflow the caller has to think about.
fn sum(counts: impl Iterator<Item = u64>) -> u64 {
    counts.fold(0, u64::saturating_add)
}

/// One column's span out of a per column list, or zero when the list is shorter than the column.
fn span_bytes(spans: &[Span], at: usize) -> u64 {
    spans.get(at).map_or(0, |span| u64::from(span.length))
}

/// One column's page out of a per column list, or zero when that column has no page at all.
fn page_bytes(pages: &[Option<Page>], at: usize) -> u64 {
    pages.get(at).and_then(Option::as_ref).map_or(0, Page::bytes)
}

/// Everything one column's global dictionary costs the file, its page and the blocks outside it.
fn dictionary_bytes(table: &Table, at: usize) -> u64 {
    page_bytes(&table.dictionaries, at)
        .saturating_add(table.dictionary_payloads.get(at).copied().unwrap_or(0))
}

/// The xxHash64 of `bytes`, which is what every span this format stores is checked against.
///
/// It walks the input as chunks rather than as offsets into it, and that is the only thing about it
/// worth a comment. The offset form reads `bytes[at..at + 8]`, and neither the slicing nor the
/// `try_into` behind it can be proved in range by a compiler that does not know where `at` stopped,
/// so each of the four lanes paid for a bounds check and a length check on every thirty two bytes.
/// A chunk carries its own length, so both fold away and the loop is the multiplies and rotates it
/// was meant to be. That loop runs over every byte of every span a query reads, which on ClickBench
/// 8 is about five percent of the query.
fn checksum(bytes: &[u8]) -> u64 {
    seeded_checksum(bytes, 0)
}

/// A hundred and twenty eight bit name for `bytes`, as two xxHash64 walks under different seeds,
/// with the format this build writes folded in so that a name made by one format is never taken
/// for the name of a file in another.
///
/// For a caller outside this crate that has to name a file by what went into it, which is what a
/// Parquet mirror's key is. See the global dictionary's use of the same pair for the arithmetic.
#[must_use]
pub fn content_name(bytes: &[u8]) -> u128 {
    let seed = u64::from(FORMAT);
    u128::from(seeded_checksum(bytes, seed)) << 64 | u128::from(seeded_checksum(bytes, !seed))
}

/// [`content_name`] of bytes that arrive in pieces, which gives the same name as the pieces joined.
///
/// A Parquet mirror is named for the file's footer, and that is 930 KB on the ten million row
/// ClickBench file. Read whole to be hashed it is a freed megabyte in every process that opens the
/// mirror, which the allocator keeps. Read a window at a time it is a window.
#[derive(Debug, Clone)]
pub struct ContentNamer {
    seeds: [u64; 2],
    lanes: [[u64; 4]; 2],
    held: [u8; 32],
    filled: usize,
    length: u64,
}

impl Default for ContentNamer {
    fn default() -> Self {
        let seed = u64::from(FORMAT);
        let seeds = [seed, !seed];
        let lanes = seeds.map(|seed| {
            [
                seed.wrapping_add(XXH_P1).wrapping_add(XXH_P2),
                seed.wrapping_add(XXH_P2),
                seed,
                seed.wrapping_sub(XXH_P1),
            ]
        });
        Self { seeds, lanes, held: [0; 32], filled: 0, length: 0 }
    }
}

impl ContentNamer {
    /// Takes the next piece.
    pub fn update(&mut self, mut bytes: &[u8]) {
        self.length += bytes.len() as u64;
        if self.filled > 0 {
            let take = (32 - self.filled).min(bytes.len());
            self.held[self.filled..self.filled + take].copy_from_slice(&bytes[..take]);
            self.filled += take;
            bytes = &bytes[take..];
            if self.filled < 32 {
                return;
            }
            let block = self.held;
            self.lanes.iter_mut().for_each(|lanes| checksum_block(lanes, &block));
            self.filled = 0;
        }
        let mut blocks = bytes.chunks_exact(32);
        for block in blocks.by_ref() {
            self.lanes.iter_mut().for_each(|lanes| checksum_block(lanes, block));
        }
        let rest = blocks.remainder();
        self.held[..rest.len()].copy_from_slice(rest);
        self.filled = rest.len();
    }

    /// The name of everything taken so far.
    #[must_use]
    pub fn finish(&self) -> u128 {
        let rest = &self.held[..self.filled];
        let [first, second] = [0, 1].map(|at| {
            if self.length < 32 {
                checksum_tail(self.seeds[at].wrapping_add(XXH_P5).wrapping_add(self.length), rest)
            } else {
                finish_checksum(self.lanes[at], rest, self.length)
            }
        });
        u128::from(first) << 64 | u128::from(second)
    }
}

/// The xxHash64 of `bytes` started from `seed`, which is the same walk with a different beginning.
///
/// A seed is here for one caller: a global dictionary decides whether two values are the same by
/// their hashes rather than by their bytes, and one sixty four bit hash is not enough to do that
/// with. Twenty million distinct values collide on sixty four bits about once in a hundred thousand
/// loads, which for a wrong answer is far too often. Two hashes of the same value under different
/// seeds are independent, so the pair is a hundred and twenty eight bits and the same arithmetic
/// puts that at around one in 1e24.
fn seeded_checksum(bytes: &[u8], seed: u64) -> u64 {
    // Asked for before the loop rather than after it, because a `ChunksExact` settles what it
    // cannot divide when it is built and hands back the same tail whether it has been walked or not.
    let mut blocks = bytes.chunks_exact(32);
    let rest = blocks.remainder();
    if bytes.len() < 32 {
        return checksum_tail(seed.wrapping_add(XXH_P5).wrapping_add(bytes.len() as u64), rest);
    }
    let mut lanes = [
        seed.wrapping_add(XXH_P1).wrapping_add(XXH_P2),
        seed.wrapping_add(XXH_P2),
        seed,
        seed.wrapping_sub(XXH_P1),
    ];
    for block in blocks.by_ref() {
        checksum_block(&mut lanes, block);
    }
    finish_checksum(lanes, rest, bytes.len() as u64)
}

const XXH_P1: u64 = 11_400_714_785_074_694_791;
const XXH_P2: u64 = 14_029_467_366_897_019_727;
const XXH_P3: u64 = 1_609_587_929_392_839_161;
const XXH_P4: u64 = 9_650_029_242_287_828_579;
const XXH_P5: u64 = 2_870_177_450_012_600_261;

fn checksum_round(state: u64, word: u64) -> u64 {
    state.wrapping_add(word.wrapping_mul(XXH_P2)).rotate_left(31).wrapping_mul(XXH_P1)
}

fn checksum_word(chunk: &[u8]) -> u64 {
    u64::from_le_bytes(chunk.try_into().expect("eight checksum bytes"))
}

/// One thirty two byte block into the four lanes.
fn checksum_block(lanes: &mut [u64; 4], block: &[u8]) {
    for (lane, chunk) in lanes.iter_mut().zip(block.chunks_exact(8)) {
        *lane = checksum_round(*lane, checksum_word(chunk));
    }
}

/// The lanes after every whole block, folded together with what was left over and the length.
fn finish_checksum(lanes: [u64; 4], rest: &[u8], length: u64) -> u64 {
    let merge = |state: u64, lane: u64| {
        (state ^ checksum_round(0, lane)).wrapping_mul(XXH_P1).wrapping_add(XXH_P4)
    };
    let [one, two, three, four] = lanes;
    let combined = one
        .rotate_left(1)
        .wrapping_add(two.rotate_left(7))
        .wrapping_add(three.rotate_left(12))
        .wrapping_add(four.rotate_left(18));
    let hash = merge(merge(merge(merge(combined, one), two), three), four);
    checksum_tail(hash.wrapping_add(length), rest)
}

/// The fewer than thirty two bytes after the last whole block, and the final mix.
fn checksum_tail(mut hash: u64, mut rest: &[u8]) -> u64 {
    let mut words = rest.chunks_exact(8);
    for chunk in words.by_ref() {
        hash ^= checksum_round(0, checksum_word(chunk));
        hash = hash.rotate_left(27).wrapping_mul(XXH_P1).wrapping_add(XXH_P4);
    }
    rest = words.remainder();
    if rest.len() >= 4 {
        let (head, tail) = rest.split_at(4);
        let quarter = u32::from_le_bytes(head.try_into().expect("four checksum bytes"));
        hash ^= u64::from(quarter).wrapping_mul(XXH_P1);
        hash = hash.rotate_left(23).wrapping_mul(XXH_P2).wrapping_add(XXH_P3);
        rest = tail;
    }
    for &byte in rest {
        hash ^= u64::from(byte).wrapping_mul(XXH_P5);
        hash = hash.rotate_left(11).wrapping_mul(XXH_P1);
    }
    hash ^= hash >> 33;
    hash = hash.wrapping_mul(XXH_P2);
    hash ^= hash >> 29;
    hash = hash.wrapping_mul(XXH_P3);
    hash ^ (hash >> 32)
}

/// The checksum of `length` bytes of `file` from `offset`, read [`DIRECTORY_WINDOW`] at a time.
///
/// The same xxHash64 as [`checksum`], carried across reads rather than over one buffer, so that a
/// directory can be checked without all of it being in memory at once. The four lanes take whole
/// thirty two byte blocks, and a read that ends partway through one keeps the tail for the next.
fn file_checksum(file: &File, offset: u64, length: usize) -> Result<u64> {
    walk_checksummed(file, offset, length, DIRECTORY_WINDOW, |_| Ok(()))
}

/// Reads `length` bytes at `offset` a window at a time, hands each window to `each`, and answers
/// the checksum of all of them.
///
/// `window` is a multiple of thirty two, so every window but the last is whole blocks of the hash
/// and nothing has to be carried from one read to the next.
fn walk_checksummed(
    file: &File,
    offset: u64,
    length: usize,
    window: usize,
    mut each: impl FnMut(&[u8]) -> Result<()>,
) -> Result<u64> {
    debug_assert!(window % 32 == 0 && window > 0, "a window is whole blocks of the hash");
    if length < 32 {
        let mut bytes = vec![0; length];
        read_at(file, offset, &mut bytes)?;
        each(&bytes)?;
        return Ok(checksum(&bytes));
    }
    let mut lanes = [XXH_P1.wrapping_add(XXH_P2), XXH_P2, 0, 0_u64.wrapping_sub(XXH_P1)];
    let mut buffer = vec![0; window.min(length)];
    let mut read = 0;
    let (mut whole, mut filled) = (0, 0);
    while read < length {
        filled = buffer.len().min(length - read);
        read_at(file, offset + read as u64, &mut buffer[..filled])?;
        read += filled;
        each(&buffer[..filled])?;
        whole = filled / 32 * 32;
        for block in buffer[..whole].chunks_exact(32) {
            checksum_block(&mut lanes, block);
        }
    }
    Ok(finish_checksum(lanes, &buffer[whole..filled], length as u64))
}

#[derive(Debug, Clone, Copy)]
struct Slot {
    offset: u64,
    length: u32,
    generation: u64,
    hash: u64,
}

impl Slot {
    fn bytes(self) -> [u8; SLOT_BYTES] {
        let mut result = [0; SLOT_BYTES];
        result[..8].copy_from_slice(&self.offset.to_le_bytes());
        result[8..12].copy_from_slice(&self.length.to_le_bytes());
        result[12..20].copy_from_slice(&self.generation.to_le_bytes());
        result[20..28].copy_from_slice(&self.hash.to_le_bytes());
        result
    }

    fn read(bytes: &[u8]) -> Self {
        Self {
            offset: u64::from_le_bytes(bytes[..8].try_into().expect("eight bytes")),
            length: u32::from_le_bytes(bytes[8..12].try_into().expect("four bytes")),
            generation: u64::from_le_bytes(bytes[12..20].try_into().expect("eight bytes")),
            hash: u64::from_le_bytes(bytes[20..28].try_into().expect("eight bytes")),
        }
    }
}

#[derive(Debug, Clone, Copy)]
struct Page {
    offset: u64,
    length: u32,
    hash: u64,
}

impl Page {
    /// How much of the file this page takes, for [`Reader::layout`].
    fn bytes(&self) -> u64 {
        u64::from(self.length)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
enum FrequencyValue {
    Null,
    Integer(i128),
    Code(u32),
}

/// A table keyed by the sixty four bits of the values the numeric frequency pass counts.
///
/// Every integer of every numeric column goes through one of these at least once when a table
/// closes, and with the standard hasher that was a fifth of the close on its own, all of it SipHash
/// guarding against an attacker who would have to choose the rows of the file being written.
type FrequencyMap<V> = HashMap<u64, V, Spread>;

/// The first pass of [`Writer::numeric_frequency`]: a Misra-Gries candidate table keyed by a value's
/// sixty four bits, with the null counted beside it.
///
/// The table is an open addressed one of its own rather than a `HashMap`. On a column that is near
/// unique, which `hits` has a dozen of, nearly every row is a value the table has not seen, and a
/// `HashMap` spent a lookup and then a second hash and probe to insert it, and a `retain` over every
/// bucket each time the table filled. Those were 6 percent of the CPU of loading the 10m ClickBench
/// file, and the slowest of those columns decided how long the whole frequency step took. Here a
/// value is found or given the empty slot it stopped at in one probe, and a decrement rebuilds the
/// table from the few candidates that outlive it.
///
/// What the table holds after a stream of rows is the same set of counts either way, since that is
/// fixed by the algorithm and not by where the counts live.
#[derive(Debug)]
struct Candidates {
    /// A power of two number of slots, at most half of them in use. A count of zero is an empty
    /// slot, which no candidate ever is, because one whose count reaches zero is dropped.
    slots: Vec<Candidate>,
    held: usize,
    nulls: u32,
    decrements: u64,
    /// The candidates that outlive a decrement, kept so that each decrement is not an allocation.
    survivors: Vec<Candidate>,
}

/// One slot of [`Candidates`], the value's bits beside its count so a probe reads one line.
#[derive(Debug, Default, Clone, Copy)]
struct Candidate {
    bits: u64,
    count: u32,
}

/// The slots a candidate table starts with, grown by doubling as it fills.
const FIRST_CANDIDATE_SLOTS: usize = 64;

impl Default for Candidates {
    fn default() -> Self {
        Self {
            slots: vec![Candidate::default(); FIRST_CANDIDATE_SLOTS],
            held: 0,
            nulls: 0,
            decrements: 0,
            survivors: Vec::new(),
        }
    }
}

impl Candidates {
    /// Counts `times` rows of `bits` and ends in the state `times` rows counted one at a time would.
    ///
    /// A value already held, or one there is room to hold, takes the whole run at once, because
    /// every row after the first would find it held. A value the full table turns away goes a row
    /// at a time, because each of its rows decrements every candidate and one of those decrements
    /// can free the place the next row takes.
    fn add(&mut self, bits: Option<u64>, mut times: u32) {
        while times > 0 {
            let room = self.held + usize::from(self.nulls != 0) < FREQUENCY_CANDIDATES;
            match bits {
                Some(bits) => {
                    let (at, found) = self.find(bits);
                    if found {
                        self.slots[at].count = self.slots[at].count.saturating_add(times);
                        return;
                    }
                    if room {
                        self.place(at, bits, times);
                        return;
                    }
                }
                None if self.nulls != 0 => {
                    self.nulls = self.nulls.saturating_add(times);
                    return;
                }
                None if room => {
                    self.nulls = times;
                    return;
                }
                None => {}
            }
            self.decrement();
            times -= 1;
        }
    }

    /// The slot holding `bits` and `true`, or the empty slot a search for it stopped at and `false`.
    fn find(&self, bits: u64) -> (usize, bool) {
        let mask = self.slots.len() - 1;
        let mut at = home(bits, self.slots.len());
        loop {
            let slot = self.slots[at];
            if slot.count == 0 {
                return (at, false);
            }
            if slot.bits == bits {
                return (at, true);
            }
            at = (at + 1) & mask;
        }
    }

    /// Where `bits` is held, for the recount, which reads the table without changing it.
    fn position(&self, bits: u64) -> Option<usize> {
        match self.find(bits) {
            (at, true) => Some(at),
            (_, false) => None,
        }
    }

    /// Puts a new candidate in the empty slot `at`, which a search for it just stopped at, doubling
    /// the table first when that would fill more than half of it.
    fn place(&mut self, at: usize, bits: u64, count: u32) {
        let at = if (self.held + 1) * 2 > self.slots.len() {
            let wider = self.slots.len() * 2;
            let old = std::mem::replace(&mut self.slots, vec![Candidate::default(); wider]);
            for slot in old.into_iter().filter(|slot| slot.count != 0) {
                let (to, _) = self.find(slot.bits);
                self.slots[to] = slot;
            }
            self.find(bits).0
        } else {
            at
        };
        self.slots[at] = Candidate { bits, count };
        self.held += 1;
    }

    /// Takes one from every candidate and the null, dropping the ones that reach zero.
    fn decrement(&mut self) {
        let mut survivors = std::mem::take(&mut self.survivors);
        survivors.clear();
        survivors.extend(
            self.slots
                .iter()
                .filter(|slot| slot.count > 1)
                .map(|slot| Candidate { bits: slot.bits, count: slot.count - 1 }),
        );
        self.slots.fill(Candidate::default());
        self.held = survivors.len();
        for &slot in &survivors {
            let (at, _) = self.find(slot.bits);
            self.slots[at] = slot;
        }
        self.survivors = survivors;
        self.nulls = self.nulls.saturating_sub(1);
        self.decrements = self.decrements.saturating_add(1);
    }

    /// Every candidate's bits and count, in no particular order.
    fn pairs(&self) -> impl Iterator<Item = (u64, u32)> + '_ {
        self.slots.iter().filter(|slot| slot.count != 0).map(|slot| (slot.bits, slot.count))
    }
}

/// The slot a search for `bits` starts at in a table of `slots`, a power of two.
///
/// The top bits of a multiply by the golden ratio, which every bit of the value reaches, so a
/// timestamp column whose values are all multiples of a million still spreads over the table.
fn home(bits: u64, slots: usize) -> usize {
    (bits.wrapping_mul(0x9E37_79B9_7F4A_7C15) >> (64 - slots.trailing_zeros())) as usize
}

/// Equal rows in a row, gathered so they are counted once.
#[derive(Debug, Default)]
struct Run {
    bits: Option<u64>,
    times: u32,
}

impl Run {
    /// Adds one row, and hands back the run it ended if it was not the same value.
    fn push(&mut self, bits: Option<u64>) -> Option<(Option<u64>, u32)> {
        if self.times != 0 && self.bits == bits && self.times < u32::MAX {
            self.times += 1;
            return None;
        }
        let ended = self.take();
        self.bits = bits;
        self.times = 1;
        ended
    }

    /// The run being gathered, if there is one, leaving none.
    fn take(&mut self) -> Option<(Option<u64>, u32)> {
        let times = std::mem::take(&mut self.times);
        (times != 0).then_some((self.bits, times))
    }
}

/// Builds the hasher for [`FrequencyMap`].
#[derive(Debug, Default, Clone, Copy)]
struct Spread;

impl std::hash::BuildHasher for Spread {
    type Hasher = SpreadHasher;

    fn build_hasher(&self) -> SpreadHasher {
        SpreadHasher(0)
    }
}

/// Folds each word in with a full width multiply whose two halves are xored together.
///
/// A plain multiply leaves the low bits of the hash as poor as the low bits of the key, and the
/// table picks its bucket from the low bits, so a timestamp column, whose values are all multiples
/// of a million microseconds, would pile into a sixty fourth of the buckets. Folding the high half
/// of the product back in is what gives the low bits the whole word.
#[derive(Debug)]
struct SpreadHasher(u64);

impl SpreadHasher {
    fn mix(&mut self, word: u64) {
        let product = u128::from(self.0 ^ word) * 0x9E37_79B9_7F4A_7C15_u128;
        self.0 = (product as u64) ^ ((product >> 64) as u64);
    }
}

impl std::hash::Hasher for SpreadHasher {
    fn write(&mut self, bytes: &[u8]) {
        for part in bytes.chunks(8) {
            let mut word = [0; 8];
            word[..part.len()].copy_from_slice(part);
            self.mix(u64::from_le_bytes(word));
        }
    }

    fn write_u32(&mut self, value: u32) {
        self.mix(u64::from(value));
    }

    fn write_u64(&mut self, value: u64) {
        self.mix(value);
    }

    fn write_i128(&mut self, value: i128) {
        self.mix(value as u64);
        self.mix((value >> 64) as u64);
    }

    fn write_isize(&mut self, value: isize) {
        self.mix(value as u64);
    }

    fn finish(&self) -> u64 {
        self.0
    }
}

#[derive(Debug, Clone)]
struct FrequencyEntry {
    value: FrequencyValue,
    count: u64,
}

/// Exact leading frequencies for one column.
///
/// Values outside `entries` occur at most `omitted_max` times. This lets a count-descending TopN
/// use the synopsis only when its last winner is strictly above every omitted value.
#[derive(Debug, Clone)]
struct FrequencySummary {
    entries: Vec<FrequencyEntry>,
    omitted_max: u64,
    ordinals: Vec<u64>,
    ordinal_entries: Vec<u16>,
}

#[derive(Debug, Clone)]
struct PairFrequencyEntry {
    first_entry: u16,
    second: Option<u32>,
    count: u64,
}

/// Exact leading counts for one numeric frequency anchor and one stable string code space.
///
/// `omitted_max` covers both first-key values outside the numeric synopsis and pairs below the
/// retained prefix. A TopN may therefore use the entries only when its boundary strictly exceeds
/// this number.
#[derive(Debug, Clone)]
struct PairFrequencySummary {
    first: u16,
    second: u16,
    entries: Vec<PairFrequencyEntry>,
    omitted_max: u64,
}

/// One column's frequency synopsis, in memory or left where it is in the file.
///
/// A writer holds what it counted. A reader leaves every synopsis in the file and reads one back
/// when a query asks about its column, because they are the largest thing in a directory once they
/// are decoded, forty eight bytes an entry and nearly twenty thousand entries over `hits`, and
/// most queries ask about none of them. Where one sits is found at open, by reading it through and
/// checking it, so a torn synopsis is still refused when the table is opened.
#[derive(Debug, Clone)]
enum Frequencies {
    Held(FrequencySummary),
    /// Where the synopsis sits, and whether it was written with the value of each ordinal, which
    /// is what the directory's frequency magic says and the synopsis itself does not.
    Stored {
        span: Span,
        values: bool,
    },
}

/// The values one column's frequency synopsis lists, with a bound on everything it left out.
///
/// What [`Reader::frequency_prefix`] answers. The counts are exact, and `omitted_max` is how many
/// rows any value not in the list can hold, which is zero when nothing was left out at all.
#[derive(Debug, Clone)]
pub struct FrequencyPrefix {
    /// Every value the synopsis lists, with the number of rows holding it, count descending.
    pub entries: Vec<(Value, u64)>,
    /// How many rows the most common value outside the list holds, and zero for a complete list.
    pub omitted_max: u64,
}

/// Sparse row ordinals covered by a numeric frequency candidate set.
#[derive(Debug, Clone, PartialEq)]
pub struct FrequencyOccurrences {
    /// Upper bound for the frequency of every value absent from the fetched rows.
    pub omitted_max: u64,
    /// Table-wide row ordinals in ascending order.
    pub ordinals: Vec<u64>,
    /// The retained heavy-hitter values named by `anchor_indices`.
    pub anchors: Vec<Value>,
    /// The index in `anchors` at each ordinal, or empty for a legacy FQ2 directory.
    pub anchor_indices: Vec<u16>,
}

/// Exact grouped counts for a pair of values, in descending count order.
pub type PairFrequencyCounts = Vec<(Vec<Value>, u64)>;

/// Where one column's page for one stripe sits in the file.
///
/// A column page has no checksum of its own because every part inside it carries one, and the
/// stripe's index page holds those. Checking a part on the way out of the page covers exactly the
/// bytes a reader is about to decode, and covers them once whether the reader took the whole page
/// or pulled one part out of the middle of it.
#[derive(Debug, Clone, Copy, Default)]
struct Span {
    offset: u64,
    length: u32,
}

/// One optional page for each column of a stripe, holding only the pages that are there.
///
/// A stripe has three of these, the membership, sieve and part range pages. As a
/// `Vec<Option<Page>>` each was thirty two bytes a column whether the page was there or not, and
/// over the ten million rows of `hits` that is half a megabyte at open for 7171 pages out of 16380
/// slots. Kept sparse and packed, a page that is there is twenty four bytes and one that is not is
/// nothing.
#[derive(Debug, Clone, Default)]
struct Pages {
    columns: usize,
    held: Box<[StripePage]>,
}

/// A page and the column it is for, packed so that the column sits where the padding was.
#[derive(Debug, Clone, Copy)]
struct StripePage {
    offset: u64,
    hash: u64,
    length: u32,
    column: u32,
}

impl Pages {
    /// The pages of `columns` columns, one slot each in column order.
    fn from_slots(slots: Vec<Option<Page>>) -> Result<Self> {
        let mut held = Vec::with_capacity(slots.iter().flatten().count());
        for (column, page) in slots.iter().enumerate() {
            if let Some(page) = page {
                let column =
                    u32::try_from(column).map_err(|_| invalid("too many columns for a page"))?;
                held.push(StripePage {
                    offset: page.offset,
                    hash: page.hash,
                    length: page.length,
                    column,
                });
            }
        }
        Ok(Self { columns: slots.len(), held: held.into_boxed_slice() })
    }

    /// The page of one column, if it has one.
    fn get(&self, column: usize) -> Option<Page> {
        let at = self.held.binary_search_by_key(&column, |placed| placed.column as usize).ok()?;
        let placed = self.held[at];
        Some(Page { offset: placed.offset, length: placed.length, hash: placed.hash })
    }

    /// One slot per column, in column order, the way the directory writes them.
    fn slots(&self) -> impl Iterator<Item = Option<Page>> + '_ {
        (0..self.columns).map(|column| self.get(column))
    }

    /// How much of the file one column's page takes, or zero when it has none.
    fn bytes(&self, column: usize) -> u64 {
        self.get(column).map_or(0, |page| page.bytes())
    }
}

/// One independently readable stripe of a table.
#[derive(Debug, Clone)]
pub struct Stripe {
    rows: usize,
    /// Rows in each part, in source order. Kept in the directory so that mapping a row ordinal to a
    /// part, which every sparse fetch does, never reads the file.
    parts: Vec<u32>,
    /// The index page: one section per column, holding a length and a checksum for every part and
    /// then a checksum of the section itself, so that a reader can pread one column's section and
    /// still know it is intact.
    index: Span,
    pages: Vec<Span>,
    memberships: Pages,
    /// One page per column holding the membership sieve of every part of the stripe, for the
    /// columns that have one. A column whose parts all declined a sieve has no page at all.
    sieves: Pages,
    /// One page per column holding the two ends and the null count of every part of the stripe.
    ///
    /// The stripe's own `zone` below covers sixty four times as many rows, and on a column that is
    /// not the one the rows are ordered by that is the difference between skipping half the file and
    /// skipping all but three percent of it. On ClickBench 24 the cutoff the answer settles at
    /// leaves eight stripes of sixteen alive and thirty parts of nine hundred and seventy four.
    ///
    /// A page per column rather than one page for the stripe, so that a query that compares one
    /// column reads the ends of that column and not of the hundred and four beside it. Read lazily
    /// for the same reason, like the sieves.
    part_ranges: Pages,
    zone: Zone,
}

impl Stripe {
    /// Number of rows in this stripe.
    #[must_use]
    pub fn rows(&self) -> usize {
        self.rows
    }

    /// Number of parts in this stripe.
    #[must_use]
    pub fn parts(&self) -> usize {
        self.parts.len()
    }

    /// The two ends and the null count of every column over the whole stripe.
    ///
    /// In the directory and so in memory, which is what makes it the one a planner can ask. The
    /// finer ones are a page per column per stripe in the file, read by [`Reader::skips`] when a
    /// scan wants to know which parts to open.
    #[must_use]
    pub fn zone(&self) -> &Zone {
        &self.zone
    }
}

/// The committed table directory.
#[derive(Debug, Clone)]
pub struct Table {
    name: String,
    fields: Vec<Field>,
    stripes: Vec<Stripe>,
    rows: usize,
    dictionaries: Vec<Option<Page>>,
    /// Bytes of each column's dictionary payload that are outside its page, which is all of them
    /// from format 27 and none of them before. See [`DICTIONARY_PAYLOADS`].
    ///
    /// Empty rather than a row of zeros on a table that has none, and read with `get` for that
    /// reason, so that a table built by hand in a test does not have to know about it.
    dictionary_payloads: Vec<u64>,
    /// The columns whose dictionary stopped taking values partway through the load, see
    /// [`DEMOTED`].
    ///
    /// Empty rather than a row of `false` on a table that has none, and read with `get`, for the
    /// same reason `dictionary_payloads` is.
    demoted: Vec<bool>,
    frequencies: Vec<Option<Frequencies>>,
    pair_frequencies: Vec<PairFrequencySummary>,
    /// String spellings aligned with each column's frequency entries.
    ///
    /// Empty for files written before `RUDBFT1`. A `None` entry is the null frequency entry; every
    /// code entry in a column named by the block has its exact bytes here.
    frequency_texts: Vec<Vec<Option<Vec<u8>>>>,
    /// Exact candidate host aggregates and an upper bound for every omitted host.
    host_groups: Option<host::HostSummary>,
    /// How many distinct values each column holds, for the columns that know.
    ///
    /// A dictionary entry is made the first time a value is seen and nothing ever removes one, so
    /// the size of the dictionary is the number of distinct values in the column. That is the whole
    /// story for a column with no null in it, and the wrong number by one for a column with a null
    /// in it, because a null row is written as the code for the empty string and makes an entry the
    /// dictionary would not otherwise have. The writer knows which case it is, since it counts the
    /// non-null rows that use each code while it builds the frequency summary, and the reader cannot
    /// work it out from the dictionary alone. So the writer settles it here.
    distincts: Vec<Option<u64>>,
    /// The order the rows of this table are meant to be stored in, if anybody declared one.
    ///
    /// A declaration and not a measurement. Nothing here checks that the stripes actually arrived
    /// in this order, and the reason it is worth storing anyway is that the order is the only thing
    /// about a table that a rewrite destroys without anybody noticing. The fragment ranges prune on
    /// whatever order the rows came in, so a table loaded sorted prunes and the same table after a
    /// checkpoint that did not know to keep the order quietly stops pruning and nothing says why.
    clustering: Option<Clustering>,
    /// The file generation of the commit that last wrote this table's column pages.
    ///
    /// This is what spec/graph/03-the-file-format.md section 3.2 calls the table generation, and
    /// the definition is deliberately about the pages rather than about the directory. A graph
    /// section is a restatement of a column in terms of row ids, so what invalidates one is the
    /// rows being renumbered, and nothing else. Adding a second table to the file, or attaching a
    /// section to this one, commits a new file generation without touching a single row of this
    /// table, and a definition that moved with those would declare every section in the file stale
    /// for no reason.
    ///
    /// Zero on a table written before format 23, where nothing recorded it. Real generations start
    /// at one, so zero can never match a section's stamp, and a table from format 22 has no
    /// sections for it to match anyway.
    generation: u64,
    /// The graph sections this table carries, per spec/graph/03-the-file-format.md section 3.2.
    ///
    /// Empty for every table written before the section table existed, and empty is not a
    /// degraded state: section 3.1 says deleting every graph section from a file changes no answer,
    /// only the time, so a table with none here answers every query the same way and slower. That
    /// is what lets this field arrive without a migration.
    sections: Vec<Section>,
}

impl Table {
    /// The SQL table name held by this snapshot.
    #[must_use]
    pub fn name(&self) -> &str {
        &self.name
    }

    /// Columns in their SQL order.
    #[must_use]
    pub fn fields(&self) -> &[Field] {
        &self.fields
    }

    /// Committed row count.
    #[must_use]
    pub fn rows(&self) -> usize {
        self.rows
    }

    /// Independently readable stripes.
    #[must_use]
    pub fn stripes(&self) -> &[Stripe] {
        &self.stripes
    }

    /// The order the rows are meant to be stored in, if this table was declared with one.
    #[must_use]
    pub fn clustering(&self) -> Option<&Clustering> {
        self.clustering.as_ref()
    }

    /// The generation every section of this table is judged against.
    ///
    /// See the field. A caller deciding whether to read a section asks [`Section::usable`] with
    /// this.
    #[must_use]
    pub fn generation(&self) -> u64 {
        self.generation
    }

    /// Every graph section this table names, including the kinds this build does not know.
    ///
    /// Including them is the point. A caller that wants only the ones it can use asks
    /// [`Section::usable`], and a caller rewriting the directory carries the rest through, so a
    /// file opened by an older build and written again does not silently lose a section that build
    /// had no name for.
    #[must_use]
    pub fn sections(&self) -> &[Section] {
        &self.sections
    }
}

/// One table's line in the catalog directory.
///
/// The small level of the two. It holds what opening a database needs and nothing else: the name to
/// bind, the shape to plan against, the row count, and where the table's own directory sits. A file
/// of eight tables is eight of these, and reading them costs the same whether the tables hold a
/// thousand rows or a billion.
///
/// The name, the fields and the row count are repeated here rather than pointed at inside the table
/// directory, which is the entire point of having two levels. A catalog that pointed at them would
/// have to read every table directory at open to answer what tables there are, which is the cost
/// this level exists to avoid.
#[derive(Debug, Clone)]
struct Entry {
    name: String,
    fields: Vec<Field>,
    rows: usize,
    /// Where this table's own directory sits, with the checksum it was committed under.
    directory: Page,
    /// Legacy nonzero counts. New files leave these empty and derive filtered counts from
    /// reusable column frequencies when a query needs them.
    nonzero: Vec<Option<u64>>,
    /// Exact sum and non-null count for signed integer columns.
    aggregates: Vec<Option<(i128, u64)>>,
    /// Exact non-null distinct values when the writer finished counting the column.
    distincts: Vec<Option<u64>>,
    /// Exact integer or date bounds; the inner `None` means every row is null.
    extremes: Vec<StoredIntegerExtremes>,
    /// Complete bounded numeric frequencies, including NULL when present.
    frequencies: Vec<StoredNumericFrequencies>,
}

type StoredIntegerExtremes = Option<Option<(i128, i128)>>;
type StoredNumericFrequencies = Option<NumericFrequencies>;

/// One view's line in the catalog directory.
///
/// A view has no pages, so unlike a table it is entirely here and there is no second level under it.
/// What it is made of is text: the body the binder binds again at every reference, and the whole
/// statement written back out, which is what `duckdb_views()` reports and nothing else reads.
///
/// The columns are a cache and they are written down anyway, which is worth saying out loud because
/// a cache in a file looks like a mistake. It is what the pin does. Create a view on a file, open
/// the file again in another process, and `duckdb_views()` answers `column_count` and `is_bound`
/// true without anything having bound the body, so the list survived the write. Not writing it
/// would answer null and false there, and the only way back would be to bind every view at open,
/// which is the thing the cache exists to avoid.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ViewEntry {
    /// The view's own name, without the schema, the way a table entry holds its name.
    pub name: String,
    /// The query the view stands for, as the text that was written.
    pub sql: String,
    /// The whole `CREATE VIEW` written back out.
    pub statement: String,
    /// The column names the statement gave, which rename a prefix of what the body produces.
    pub aliases: Vec<String>,
    /// The columns the last bind of the body produced.
    pub columns: Vec<Field>,
}

/// Where one column's bytes went, taken from the directory rather than by reading pages.
#[derive(Debug, Clone)]
pub struct ColumnLayout {
    /// The column's name, so a report does not have to carry the field list beside this.
    pub name: String,
    /// The type, spelled the way the catalog spells it.
    pub kind: String,
    /// Every stripe's page of this column added up, which is the encoded data itself.
    pub pages: u64,
    /// Every stripe's exact code membership page for this column.
    pub memberships: u64,
    /// Every stripe's membership sieve page for this column.
    pub sieves: u64,
    /// Every stripe's per part range page for this column.
    pub part_ranges: u64,
    /// The table wide dictionary of this column, if it has one.
    pub dictionary: u64,
}

impl ColumnLayout {
    /// Everything this column costs, which is what the file would lose if the column went.
    #[must_use]
    pub fn total(&self) -> u64 {
        self.pages
            .saturating_add(self.memberships)
            .saturating_add(self.sieves)
            .saturating_add(self.part_ranges)
            .saturating_add(self.dictionary)
    }
}

/// Where a whole file's bytes went.
///
/// Every number here comes out of the committed directory, so taking it costs one directory read
/// however large the file is. That is the point: a 45 GB table has to be able to say where it went
/// without being read, or nobody will ask.
///
/// The parts that are not a column are kept apart rather than shared out over the columns. The
/// stripe index page holds a section per column and could be split, and the directory and the
/// header cannot be, so splitting one of the three and not the others would read as if the columns
/// accounted for everything. They do not, and the gap is the thing worth looking at.
#[derive(Debug, Clone)]
pub struct Layout {
    /// The size of the file on disk.
    pub file: u64,
    /// Committed rows.
    pub rows: usize,
    /// Committed stripes.
    pub stripes: usize,
    /// Committed parts, which is how many chunks a scan reads.
    pub parts: usize,
    /// One entry per column, in the table's column order.
    pub columns: Vec<ColumnLayout>,
    /// Every stripe's index page, which carries a length and a checksum for every part of every
    /// column and is charged per stripe rather than per column.
    pub indexes: u64,
    /// The committed directory itself, the one that was read to build this.
    pub directory: u64,
    /// The fixed header, which holds the magic, the format and the two directory slots.
    pub header: u64,
}

impl Layout {
    /// Everything the columns cost together.
    #[must_use]
    pub fn columns_total(&self) -> u64 {
        self.columns.iter().map(ColumnLayout::total).fold(0, u64::saturating_add)
    }

    /// What the file holds that this does not account for.
    ///
    /// A committed file is written once and never rewritten in place, so an earlier directory and
    /// the pages of an earlier snapshot are still in it. That is the honest place for them: they
    /// are bytes on disk that no column owns.
    #[must_use]
    pub fn unaccounted(&self) -> u64 {
        self.file
            .saturating_sub(self.columns_total())
            .saturating_sub(self.indexes)
            .saturating_sub(self.directory)
            .saturating_sub(self.header)
    }
}

/// How one part of one column is stored, which is one row of `pragma_storage_info`.
///
/// Everything here is read off the file rather than worked out from the schema, because the whole
/// question this answers is what the encoder chose, and the encoder chooses per part. Two files
/// holding the same rows in a different order give different answers and that difference is the
/// reason to ask.
///
/// The encoding costs a read of the column's page, so this is not free the way [`Layout`] is. It is
/// one read per column per stripe rather than one per part, because a part is a few kilobytes out
/// of a page that is a quarter of a megabyte.
#[derive(Debug, Clone)]
pub struct StoredPart {
    /// Which stripe the part belongs to.
    pub stripe: usize,
    /// Which part of that stripe it is, counting from zero inside the stripe.
    pub part: usize,
    /// The table wide row number the part starts at.
    pub row: usize,
    /// How many rows it holds.
    pub rows: usize,
    /// What the encoder made of it, as a line of text like `DICT(PACKED, PACKED)`.
    pub encoding: String,
    /// The stored bytes of the part, which is what it costs in the file.
    pub bytes: u64,
    /// Where in the file the column page holding this part starts.
    pub page: u64,
    /// Where in that page the part starts.
    pub offset: u64,
    /// The smallest value the part holds, when the stored ranges say.
    pub low: Option<Value>,
    /// The largest, same.
    pub high: Option<Value>,
    /// How many of its rows are null, when the stored ranges say.
    pub nulls: Option<usize>,
}

/// Seeds the second hash a global dictionary tells its values apart by.
///
/// Any value that is not zero does, since zero is the seed [`checksum`] already uses and the point
/// is only that the two hashes of one value are not the same number. This one is the fractional part
/// of the golden ratio in sixty four bits, which is the constant everything else here is built out
/// of and is as good a nothing-up-my-sleeve number as any.
const DICTIONARY_CHECK_SEED: u64 = 11_400_714_819_323_198_485;

/// One column's table wide dictionary while the load is running.
///
/// The thing to understand about this is what it does not hold. A dictionary of `URL` at a hundred
/// million ClickBench rows has about eighteen million distinct values and 1.3 GB of bytes in them,
/// and five columns like it are twelve of the seventeen gigabytes a load of `hits` peaks at. So the
/// bytes are not kept. A value's bytes go into [`GlobalDictionary::filling`], and when that reaches
/// [`TEXT_PAYLOAD_VALUES`] values the block is sealed, handed out at the end of the merge that
/// sealed it to be encoded with the stripe's pages, and never seen in that form again. What is left
/// is the encoded block, which is two to three times smaller, and that is the same bytes the file
/// is going to hold anyway.
///
/// Two things needed the raw bytes and neither needs them now. Deciding whether a value has been
/// seen before was a hash lookup and then a comparison of the bytes, and is now a hash lookup and a
/// comparison of a second hash under a different seed, which is [`DICTIONARY_CHECK_SEED`] and the
/// argument for why that is sound. Sorting the values at the end needed all of them at once, and
/// now reads the blocks back through [`GlobalDictionary::decoded`] one column at a time, which is
/// one column's bytes rather than every column's.
///
/// The offsets going block relative comes free with it, and takes the four gigabyte wall with it.
/// They were `u32` into a per column payload, so a column could not hold more than four gigabytes of
/// values however much memory the machine had, and `URL` and `Referer` are within a small factor of
/// that at a hundred million rows. A `u32` into a block of 1,024 values is not a bound anything real
/// reaches. The stored form is unchanged, because [`encode_offsets`] was already subtracting a per
/// block base before writing.
#[derive(Debug)]
struct GlobalDictionary {
    /// Keyed by the value's hash, which is already well spread, so the maps hash it once more
    /// with a multiply rather than with SipHash. SipHash here was one percent of a ClickBench load,
    /// and every stripe's merge of a column waits on the one before it.
    primary: HashMap<u64, u32, Spread>,
    collisions: HashMap<u64, Vec<u32>, Spread>,
    /// Every value's hash under [`DICTIONARY_CHECK_SEED`], in code order.
    checks: Vec<u64>,
    /// Where every value ends inside the payload block it is in, in code order.
    ends: Vec<u32>,
    counts: Vec<u64>,
    nulls: u64,
    /// The values of the block being filled, back to back.
    filling: Vec<u8>,
    /// One conservative four-byte substring signature per encoded payload block, in block order.
    ///
    /// Made where the block is encoded rather than where it is sealed, because sealing is under the
    /// writer's lock and every byte of every value going through [`gram_bits`] was 2.9 of the 14
    /// seconds the 10m ClickBench load spent on the 32 core box.
    grams: Vec<[u8; TEXT_GRAM_BYTES]>,
    /// Blocks that have filled and not been handed out to be encoded yet, each with its block number.
    ///
    /// Empty except inside the merge that filled them, and while the column is still too small to
    /// settle a shape on.
    waiting: Vec<(usize, Vec<u8>)>,
    /// Blocks kept raw to settle a shape on, spread across the column, each with its number.
    ///
    /// At most [`PAYLOAD_SAMPLE_BLOCKS`] of them and so at most a few megabytes. Spread rather than
    /// taken off the front for the reason [`settle_shape`] gives, and kept rather than read back
    /// because reading back is a decode and this is a sample of a column that is still growing.
    sample: Vec<(usize, Vec<u8>)>,
    /// How far apart the blocks in `sample` are, which doubles every time there are too many.
    stride: usize,
    /// What the blocks encoded so far were encoded with, once the column is big enough to settle it.
    shape: Option<chooser::Settled>,
    /// How many blocks had filled when that shape was settled.
    settled: usize,
    /// The blocks that are encoded and not yet in the file, in block order, following `placed`.
    ///
    /// Empty between stripes, because [`Writer::place_blocks`] writes them the moment they come
    /// back. Only a dictionary that never meets a writer, which is a test's, keeps them here.
    blocks: Vec<Vec<u8>>,
    /// Blocks that came back encoded ahead of a block before them, by block number.
    ///
    /// Two stripes merged one after the other can have their pages built in the other order, and a
    /// block cannot go into `blocks` until every block before it is there. They wait here until the
    /// gap closes, which is at most until the stripe merged just before this one is written.
    early: BTreeMap<usize, EncodedBlock>,
    /// Where every block already written to the file is, in block order.
    placed: Vec<Placed>,
    /// What the dictionary held the last time it was asked, see [`Self::recharge`], which is also
    /// what the load profile was told when there is one.
    charged: u64,
    /// Whether the dictionary stopped taking values, see [`Self::demote`].
    demoted: bool,
}

/// Where one payload block of a global dictionary is in the file, and its checksum.
#[derive(Debug, Clone, Copy)]
struct Placed {
    start: u64,
    length: u64,
    hash: u64,
}

/// Sorted `(head, code)` entries and the decoded bytes and block bases they were sorted over.
type RankedDictionary = (Vec<(u64, u32)>, Vec<u8>, Vec<u64>);

impl GlobalDictionary {
    fn new() -> Self {
        Self {
            primary: HashMap::default(),
            collisions: HashMap::default(),
            checks: Vec::new(),
            ends: Vec::new(),
            counts: Vec::new(),
            nulls: 0,
            filling: Vec::new(),
            grams: Vec::new(),
            waiting: Vec::new(),
            sample: Vec::new(),
            stride: 1,
            shape: None,
            settled: 0,
            blocks: Vec::new(),
            early: BTreeMap::new(),
            placed: Vec::new(),
            charged: 0,
            demoted: false,
        }
    }

    /// How many distinct values this dictionary holds, which is one past its largest code.
    fn values(&self) -> usize {
        self.ends.len()
    }

    /// About how many bytes closing this dictionary holds at once: every value decoded, and a
    /// sort entry and a code for each.
    fn closing_bytes(&self) -> usize {
        let values = self.values();
        let decoded = (0..values.div_ceil(TEXT_PAYLOAD_VALUES))
            .map(|block| self.ends[((block + 1) * TEXT_PAYLOAD_VALUES).min(values) - 1] as usize)
            .sum::<usize>();
        decoded.saturating_add(values.saturating_mul(size_of::<(u64, u32)>() + size_of::<u32>()))
    }

    /// About what the dictionary holds in memory, by capacity rather than by length.
    ///
    /// A hash table is charged its buckets, which is a power of two over eight sevenths of what it
    /// says it can hold, and a byte of control per bucket. The blocks waiting to be encoded and the
    /// ones kept to settle a shape on are counted one by one, and there are only ever a few.
    fn held_bytes(&self) -> u64 {
        fn table<K, V, S>(map: &HashMap<K, V, S>) -> usize {
            (map.capacity() * 8 / 7).next_power_of_two() * (size_of::<(K, V)>() + 1)
        }
        fn spilled<T>(values: &Vec<T>) -> usize {
            values.capacity() * size_of::<T>()
        }
        let raw = |blocks: &Vec<(usize, Vec<u8>)>| {
            spilled(blocks) + blocks.iter().map(|(_, block)| block.capacity()).sum::<usize>()
        };
        let bytes = table(&self.primary)
            + table(&self.collisions)
            + self.collisions.values().map(spilled).sum::<usize>()
            + spilled(&self.checks)
            + spilled(&self.ends)
            + spilled(&self.counts)
            + self.filling.capacity()
            + spilled(&self.grams)
            + raw(&self.waiting)
            + raw(&self.sample)
            + self.blocks.iter().map(Vec::capacity).sum::<usize>()
            + spilled(&self.placed);
        bytes as u64
    }

    /// Tells `profile` what the dictionary has grown or shrunk by since the last time, and hands
    /// back what it held then and what it holds now.
    fn recharge(&mut self, profile: Option<&LoadProfile>) -> (u64, u64) {
        let before = self.charged;
        let now = self.held_bytes();
        if let Some(profile) = profile {
            if now >= before {
                profile.hold(now - before);
            } else {
                profile.release(before - now);
            }
        }
        self.charged = now;
        (before, now)
    }

    /// Stops the dictionary taking values, for good.
    ///
    /// The block being filled is sealed so that it goes out with the others, and what the
    /// dictionary keeps for looking values up is let go of, which on a column of mostly new values
    /// is most of what it holds. What stays is what the close needs to write the dictionary's page:
    /// where every value ends, how often each was seen and where its blocks went. The stripes that
    /// were coded against it still need that page to be read. See [`DEMOTED`].
    fn demote(&mut self) {
        if self.demoted {
            return;
        }
        self.seal_rest();
        self.release_lookup();
        self.demoted = true;
    }

    /// Frees what the dictionary keeps for coding new values, once none are coming.
    ///
    /// The hash tables, the check hash of every value and the blocks kept to settle a shape on are
    /// what a merge looks values up in. The close reads the counts, the ends and the written blocks
    /// and none of these, which are most of what the dictionary holds per value, so they go before
    /// the close takes memory of its own rather than after.
    fn release_lookup(&mut self) {
        self.primary = HashMap::default();
        self.collisions = HashMap::default();
        self.checks = Vec::new();
        self.sample = Vec::new();
        self.filling = Vec::new();
    }

    /// How many blocks are encoded, written or not, which is the number the next one has to have.
    fn encoded(&self) -> usize {
        self.placed.len() + self.blocks.len()
    }

    #[cfg(test)]
    fn code(&mut self, text: &str) -> Result<u32> {
        let bytes = text.as_bytes();
        self.code_hashed(bytes, checksum(bytes), seeded_checksum(bytes, DICTIONARY_CHECK_SEED))
    }

    /// The code for a value whose two hashes the caller already has.
    ///
    /// A stripe prepared outside the writer's lock hashed every value it holds while it was coding
    /// them, and merging it into this dictionary is one of these a distinct value rather than two
    /// hashes of every row. See [`prepare`].
    fn code_hashed(&mut self, text: &[u8], hash: u64, check: u64) -> Result<u32> {
        if let Some(&code) = self.primary.get(&hash) {
            if self.checks.get(code as usize) == Some(&check) {
                return Ok(code);
            }
            if let Some(codes) = self.collisions.get(&hash) {
                if let Some(code) =
                    codes.iter().copied().find(|&code| self.checks[code as usize] == check)
                {
                    return Ok(code);
                }
            }
            let code = self.insert(text, check)?;
            self.collisions.entry(hash).or_default().push(code);
            return Ok(code);
        }
        let code = self.insert(text, check)?;
        self.primary.insert(hash, code);
        Ok(code)
    }

    fn insert(&mut self, text: &[u8], check: u64) -> Result<u32> {
        if self.demoted {
            return Err(Error::internal("a value was coded against a demoted dictionary"));
        }
        let code = u32::try_from(self.ends.len())
            .map_err(|_| invalid("global dictionary has too many values"))?;
        self.filling.extend_from_slice(text);
        self.ends.push(
            u32::try_from(self.filling.len())
                .map_err(|_| invalid("a global dictionary value exceeds 4 GiB"))?,
        );
        self.checks.push(check);
        self.counts.push(0);
        if self.ends.len() % TEXT_PAYLOAD_VALUES == 0 {
            self.seal();
        }
        Ok(code)
    }

    /// Closes the block being filled and puts it in the queue to be encoded.
    ///
    /// Also keeps a copy of it if it lands on the sample's stride, and halves the sample when that
    /// has left too many, which is what keeps the kept blocks spread evenly over however much of the
    /// column exists rather than bunched at whichever end was cheap to remember.
    fn seal(&mut self) {
        let at = self.ends.len().div_ceil(TEXT_PAYLOAD_VALUES) - 1;
        let bytes = std::mem::take(&mut self.filling);
        if at % self.stride == 0 {
            self.sample.push((at, bytes.clone()));
            if self.sample.len() > PAYLOAD_SAMPLE_BLOCKS {
                self.stride *= 2;
                let stride = self.stride;
                self.sample.retain(|(at, _)| at % stride == 0);
            }
        }
        self.waiting.push((at, bytes));
    }

    /// The values of one block, as slices into the bytes the block was filled with.
    fn slices<'a>(&self, at: usize, bytes: &'a [u8]) -> Vec<&'a [u8]> {
        block_values(self.block_ends(at), bytes)
    }

    /// Where every value of one block ends, relative to the block.
    fn block_ends(&self, at: usize) -> &[u32] {
        let first = (at * TEXT_PAYLOAD_VALUES).min(self.ends.len());
        let last = (first + TEXT_PAYLOAD_VALUES).min(self.ends.len());
        &self.ends[first..last]
    }

    /// Takes every waiting block out to be encoded somewhere else, if the column has a shape to
    /// encode them with.
    ///
    /// This is what keeps the encoding out of the writer's lock. A block needs its bytes, where its
    /// values end and the shape, and nothing else of the dictionary, so it goes out with a copy of
    /// the four kilobytes of ends it has and comes back through [`GlobalDictionary::take_back`].
    fn hand_out(&mut self, column: usize) -> Vec<Unencoded> {
        let Some(shape) = &self.shape else { return Vec::new() };
        let waiting = std::mem::take(&mut self.waiting);
        waiting
            .into_iter()
            .map(|(at, bytes)| Unencoded {
                column,
                at,
                ends: self.block_ends(at).to_vec(),
                bytes,
                shape: shape.clone(),
            })
            .collect()
    }

    /// Takes back one block that was handed out, and moves every block that is now next in line
    /// into `blocks`.
    fn take_back(&mut self, at: usize, block: EncodedBlock) -> Result<()> {
        if at < self.encoded() || self.early.insert(at, block).is_some() {
            return Err(Error::internal("a dictionary block came back twice"));
        }
        while let Some(block) = self.early.remove(&self.encoded()) {
            self.push_block(block);
        }
        Ok(())
    }

    /// Appends the next encoded block and its signature.
    fn push_block(&mut self, (bytes, grams): EncodedBlock) {
        self.blocks.push(bytes);
        self.grams.push(*grams);
    }

    /// Settles the shape the waiting blocks are about to be encoded with, if there is enough column
    /// to settle one on.
    ///
    /// Settled again once the column has grown fourfold, because the sample it was settled on then
    /// covered a quarter of what exists now and a dictionary in first seen order does not look the
    /// same at both ends. Blocks already encoded keep the shape they were encoded with. They can,
    /// because a block says what it is: nothing reading one asks the column what shape to expect.
    fn settle(&mut self) -> Result<()> {
        if self.sample.len() < PAYLOAD_SAMPLE_BLOCKS {
            return Ok(());
        }
        self.settle_on_sample()
    }

    /// Settles a shape on whatever sample there is, for a column the load ended before it had
    /// enough of to settle one the usual way.
    ///
    /// Such a column has fewer than [`PAYLOAD_SAMPLE_BLOCKS`] blocks, so the sample is every block
    /// it has. Trying every candidate on each of them instead runs at two to six megabytes a second,
    /// and once `hits` stored its string columns with a dictionary, the forty or so small ones were
    /// more than half the CPU of a million row load, all of it in the close.
    fn settle_rest(&mut self) -> Result<()> {
        if self.shape.is_some() || self.sample.is_empty() {
            return Ok(());
        }
        self.settle_on_sample()
    }

    fn settle_on_sample(&mut self) -> Result<()> {
        let complete = self.ends.len() / TEXT_PAYLOAD_VALUES;
        if self.shape.is_some() && complete < self.settled.saturating_mul(4) {
            return Ok(());
        }
        let sample =
            self.sample.iter().map(|(at, bytes)| self.slices(*at, bytes)).collect::<Vec<_>>();
        self.shape = Some(string::with_symbols(settle_shape(&sample)?, &sample));
        self.settled = complete;
        Ok(())
    }

    /// Seals the part block at the end of the load, if there is one.
    fn seal_rest(&mut self) {
        // Asked of the values rather than of the bytes, because a block of empty strings has values
        // in it and no bytes, and a column of nulls is exactly that. A demoted dictionary sealed its
        // part block when it was demoted and has taken nothing since.
        if !self.demoted && self.ends.len() % TEXT_PAYLOAD_VALUES != 0 {
            self.seal();
        }
    }

    /// Encodes the waiting block at `at`, with the settled shape when there is one and by trying
    /// everything when the column was too small to settle one.
    fn encode_waiting(&self, at: usize) -> Result<EncodedBlock> {
        let (block, bytes) = &self.waiting[at];
        let values = self.slices(*block, bytes);
        let encoded = match &self.shape {
            Some(shape) => string::encode_with(&values, shape)?,
            None => string::encode(&values)?,
        };
        Ok((encoded, block_grams(&values)))
    }

    /// [`finish_dictionaries`] for one dictionary on this thread, for the tests that hold one.
    #[cfg(test)]
    fn finish_blocks(&mut self) -> Result<()> {
        self.seal_rest();
        let made = (0..self.waiting.len())
            .map(|at| self.encode_waiting(at))
            .collect::<Result<Vec<_>>>()?;
        for ((at, _), block) in std::mem::take(&mut self.waiting).into_iter().zip(made) {
            if self.encoded() != at {
                return Err(Error::internal("a dictionary block was encoded out of order"));
            }
            self.push_block(block);
        }
        Ok(())
    }

    /// Every value of this dictionary read back out of its encoded blocks, as the bytes back to back
    /// and where each block starts in them.
    ///
    /// This is the one place the whole column is in memory at once and the reason [`Writer::close`]
    /// takes the columns one at a time rather than across threads. One column's values is 1.3 GB on
    /// the worst ClickBench column, and five columns of that at once is the peak this was all meant
    /// to remove.
    ///
    /// The blocks are spread over threads instead. Each block's decoded length is already known from
    /// the ends of its values, so the answer is laid out before anything is decoded and every thread
    /// decodes its own run of blocks straight into its own part of it. On the 10m ClickBench sample
    /// this was a second of the close for `URL` alone, on one core of thirty two, and the close is
    /// what a load waits on once its stripes are written.
    ///
    /// The blocks already written are read back out of `file`, so what a thread holds beyond the
    /// answer is one encoded block. They were written moments or minutes ago and are almost always
    /// still in the page cache, so this is a copy rather than a read of the disk.
    fn decoded(&self, file: Option<&dyn rudb_io::File>) -> Result<(Vec<u8>, Vec<u64>)> {
        let count = self.placed.len() + self.blocks.len();
        if count != self.values().div_ceil(TEXT_PAYLOAD_VALUES) {
            return Err(invalid("global dictionary blocks do not cover its values"));
        }
        let mut bases = Vec::with_capacity(count);
        let mut total = 0_usize;
        for block in 0..count {
            bases.push(total as u64);
            let last = ((block + 1) * TEXT_PAYLOAD_VALUES).min(self.values()) - 1;
            total = total
                .checked_add(self.ends[last] as usize)
                .ok_or_else(|| invalid("global dictionary does not fit in memory"))?;
        }
        let mut flat = vec![0_u8; total];
        let mut outs = Vec::with_capacity(count);
        let mut rest = flat.as_mut_slice();
        for block in 0..count {
            let end = bases.get(block + 1).map_or(total, |&base| base as usize);
            let (out, after) = rest.split_at_mut(end - bases[block] as usize);
            outs.push((block, out));
            rest = after;
        }
        let one = |run: &mut [(usize, &mut [u8])]| -> Result<()> {
            let mut stored = Vec::new();
            for (block, out) in run {
                let encoded = match self.placed.get(*block) {
                    Some(place) => {
                        let file = file.ok_or_else(|| {
                            Error::internal("a written dictionary block has no file")
                        })?;
                        let length = usize::try_from(place.length).map_err(|_| {
                            invalid("global dictionary block does not fit in memory")
                        })?;
                        stored.resize(length, 0);
                        read_at(file, place.start, &mut stored)?;
                        if checksum(&stored) != place.hash {
                            return Err(invalid(
                                "a global dictionary block did not read back as written",
                            ));
                        }
                        stored.as_slice()
                    }
                    None => &self.blocks[*block - self.placed.len()],
                };
                let decoded = string::decode_flat(encoded)?;
                if decoded.bytes().len() != out.len() {
                    return Err(invalid(
                        "a global dictionary block is not the length its ends say",
                    ));
                }
                out.copy_from_slice(decoded.bytes());
            }
            Ok(())
        };
        // Sixteen blocks a thread at the least, because a thread costs about what decoding a few
        // blocks does and most columns have one or two.
        let workers = close_workers().min(count / 16).max(1);
        if workers <= 1 {
            one(&mut outs)?;
        } else {
            let per = count.div_ceil(workers);
            std::thread::scope(|scope| {
                outs.chunks_mut(per)
                    .map(|run| scope.spawn(|| one(run)))
                    .collect::<Vec<_>>()
                    .into_iter()
                    .try_for_each(|handle| {
                        handle.join().map_err(|_| {
                            Error::internal("a global dictionary decode worker panicked")
                        })?
                    })
            })?;
        }
        drop(outs);
        Ok((flat, bases))
    }

    /// Where the value at `code` sits in the bytes [`GlobalDictionary::decoded`] handed back.
    ///
    /// A block's first value starts at the block, and every other value starts where the one before
    /// it ended, which is what makes 1,024 values 1,024 numbers rather than 1,025.
    fn value_span(ends: &[u32], bases: &[u64], code: usize) -> (usize, usize) {
        let Some(&base) = bases.get(code / TEXT_PAYLOAD_VALUES) else { return (0, 0) };
        let Some(&end) = ends.get(code) else { return (0, 0) };
        let base = base as usize;
        let from = if code % TEXT_PAYLOAD_VALUES == 0 { 0 } else { ends[code - 1] as usize };
        (base + from, base + end as usize)
    }

    /// This dictionary's values in sorted order, each as the first eight bytes of the value and the
    /// code that holds it, so entry `rank` describes the value that sits at `rank` when the values
    /// are sorted by their bytes.
    ///
    /// Codes themselves stay in first appearance order, which is what lets the writer hand one out
    /// the moment it sees a value rather than waiting for the last stripe, and which also keeps a
    /// stripe's codes close together because the data is clustered. This is what puts the values
    /// back in order for anything that needs it, and it is separate from the codes so that getting
    /// it costs a sort of the distinct values at the end rather than a rewrite of every code page.
    ///
    /// The order is the byte order of the values and nothing else. The heads are attached after the
    /// sort rather than sorted on, because padding with zero on the right is order preserving for
    /// byte strings and so sorting by head and then by bytes lands in the same place as sorting by
    /// bytes: a shorter value differs from a longer one that starts the same way at a position
    /// where the shorter one has run out, and zero is below every byte that could be there.
    ///
    /// The heads are kept because a reader searching this order wants a comparison it can make out
    /// of the index alone. What they buy there depends entirely on the column and is much less than
    /// it looks on the columns that cost the most, which [`sort_by_value`] measures.
    fn ranked_with_values(&self, file: Option<&dyn rudb_io::File>) -> Result<RankedDictionary> {
        let (flat, bases) = self.decoded(file)?;
        let value = |code: u32| {
            let (from, to) = Self::value_span(&self.ends, &bases, code as usize);
            flat.get(from..to).unwrap_or_default()
        };
        let mut codes = (0..self.values() as u32).collect::<Vec<_>>();
        sort_by_value_across(&mut codes, value, close_workers());
        let order = codes.into_iter().map(|code| (head(value(code)), code)).collect();
        Ok((order, flat, bases))
    }

    #[cfg(test)]
    fn ranked(&self, file: Option<&dyn rudb_io::File>) -> Result<Vec<(u64, u32)>> {
        self.ranked_with_values(file).map(|(order, _, _)| order)
    }
}

/// Appends pages and commits a new directory.
///
/// One writer covers a whole file rather than one table. [`Writer::next`] closes the table it is on
/// and opens another over the same file, and [`Writer::finish`] commits every table it has closed in
/// one generation. That is what makes a checkpoint atomic across tables: there is one slot write at
/// the end of it and a reader sees every table at the generation before it or every table at the
/// generation after it.
#[derive(Debug)]
pub struct Writer {
    /// The file, through `rudb-io` rather than `std::fs`, so that a test can hand the writer a
    /// simulated filesystem and crash a load at every call it makes.
    file: Box<dyn rudb_io::File>,
    /// Where the next write goes, counted here rather than asked of the file.
    ///
    /// The file's own cursor is not ours. Building the numeric frequencies reads pages back through
    /// [`read_at`], and a positional read is only positional about where it reads from: `pread`
    /// leaves the cursor alone, and the call Windows has for it moves the cursor to the end of what
    /// it read. A writer that asked the file where it was would then write the directory over a
    /// page it had already written, which is what it did.
    at: u64,
    /// How far into the file the kernel has been asked to start writing, see [`WRITEBACK_STRETCH`].
    written_back: u64,
    table: Table,
    generation: u64,
    /// The first and the last source position in every stripe, in the order the stripes were
    /// written.
    order: Vec<((u64, u64), (u64, u64))>,
    next_order: u64,
    dictionaries: Vec<Option<GlobalDictionary>>,
    /// Which columns still have a global dictionary, shared with every [`Preparer`] this writer
    /// hands out so that a stripe prepared after a column lost its dictionary is not coded for it.
    coded: Arc<prepare::Coding>,
    /// One per column, folding the rows into a summary and a sketch as they go past.
    ///
    /// `None` for a column with no hash rule, which is the interval and the nested types. See
    /// [`stats::Gather`] for why the statistics are built here rather than by reading the file back
    /// once it is committed.
    gathers: Vec<Option<stats::Gather>>,
    /// The dictionaries and the statistics while a [`Merger`] has them, which is from
    /// [`Writer::merger`] until the table is closed. `dictionaries` and `gathers` are empty then.
    lent: Option<Arc<Lent>>,
    pending: Vec<PendingChunk>,
    /// The tables already closed in this generation, in the order they were written.
    closed: Vec<Entry>,
    /// The views the next commit writes down, which [`Writer::with_views`] sets.
    ///
    /// Carried forward from the committed generation by [`Writer::open`], so a writer that was only
    /// opened to append a table does not have to know about views to avoid dropping them.
    views: Vec<ViewEntry>,
    /// Where the stages this writer runs are charged, which [`Writer::with_profile`] sets.
    ///
    /// The writer runs the page builder, the dictionary blocks, the writes and the publish, and it
    /// charges them once per stripe and once per worker, never per chunk. See
    /// `rudb_metrics::LoadProfile` for why that is the grain.
    profile: Option<Arc<LoadProfile>>,
}

/// A chunk that has arrived and is waiting for the rest of its stripe.
///
/// The rows are kept rather than the pages they encode to, which is the whole of #808's first half.
/// Encoding on arrival put every column of every part on the thread that called `append_at`, and
/// that thread is the only one the load has. Encoding at the flush instead means a stripe's worth
/// of work is on the table at once, and a stripe splits by column into a hundred and five pieces
/// that share nothing.
#[derive(Debug)]
struct PendingChunk {
    order: (u64, u64),
    chunk: Chunk,
}

/// What the writer still needs of a part once its columns are encoded: where in the source it came
/// from, how many rows it has and how large those rows were.
///
/// A stripe waiting for the writer's lock carries these rather than its chunks, so its rows are
/// freed as soon as they are encoded and not after the stripe is written. See [`prepare`].
#[derive(Debug, Clone, Copy)]
struct Part {
    order: (u64, u64),
    rows: usize,
    footprint: usize,
}

impl Part {
    fn of(pending: &PendingChunk) -> Self {
        Self {
            order: pending.order,
            rows: pending.chunk.len(),
            footprint: pending.chunk.footprint(),
        }
    }
}

/// One column's share of a stripe, which is what one encode worker produces.
///
/// Indexed by part, so a stripe is a column of these and the write loop reads down one of them.
/// That is also the order the loop wanted: `flush_pending` walks a column at a time and lays its
/// parts next to each other, and it used to reach across a row of parts to do it.
#[derive(Debug, Default)]
struct ColumnStripe {
    pages: Vec<Vec<u8>>,
    codes: Vec<Option<Vec<u32>>>,
    sieves: Vec<Option<Sieve>>,
    ranges: Vec<Range>,
}

/// Whether a column of this type is coded against a global dictionary.
///
/// A dictionary, its codes and the membership index beside them are about bytes and not about
/// text, so a blob gets one the same as a varchar does. ClickBench's `hits.parquet` stores every
/// string column as a plain byte array, which reads back as a blob, and those columns were being
/// written as a length and the bytes for every row: 533 MB for the first million rows where DuckDB
/// writes 142.
fn coded_type(ty: &LogicalType) -> bool {
    matches!(ty, LogicalType::Varchar | LogicalType::Blob)
}

/// The tag a directory gives a column's global dictionary.
///
/// A varchar's is 1, as it always was. A blob's is 2, so that a reader from before blobs had
/// dictionaries meets a tag it does not know and refuses the file, rather than laying the rest of
/// the directory out as if the blob columns had no dictionary and reading everything after the
/// first one from the wrong place.
fn dictionary_tag(ty: &LogicalType) -> u8 {
    if ty == &LogicalType::Blob { 2 } else { 1 }
}

/// Roughly what encoding a column of this type costs, for ordering the encode queue.
///
/// Only the order matters and only roughly. A string column hashes and copies every value into a
/// dictionary and is in a different class from everything else, and among the fixed widths the wide
/// ones carry more bytes through the cascade than the narrow ones. Anything finer than that would
/// be a cost model, and the queue already absorbs a wrong guess: it only has to avoid finishing on
/// a column nobody else can help with.
fn weight(ty: &LogicalType) -> usize {
    match ty {
        LogicalType::Varchar | LogicalType::Blob | LogicalType::Bit => 64,
        LogicalType::HugeInt
        | LogicalType::UHugeInt
        | LogicalType::Uuid
        | LogicalType::Interval => 16,
        LogicalType::BigInt
        | LogicalType::UBigInt
        | LogicalType::Timestamp
        | LogicalType::Time
        | LogicalType::TimeTz
        | LogicalType::TimestampTz
        | LogicalType::TimestampS
        | LogicalType::TimestampMs
        | LogicalType::TimestampNs
        | LogicalType::Double
        | LogicalType::Decimal { .. } => 8,
        LogicalType::Integer | LogicalType::UInteger | LogicalType::Date | LogicalType::Float => 4,
        LogicalType::SmallInt | LogicalType::USmallInt => 2,
        _ => 1,
    }
}

/// Parts in one stripe.
///
/// Sixty four thousand rows is the smallest stripe that keeps the ClickBench directory in single
/// digit megabytes at a hundred million rows, and it puts a four byte column's page at a quarter of
/// a megabyte, which is the size a sequential read wants. Larger stripes buy a smaller directory
/// and cost a sparse fetch, which has to read a page index before it can reach one part.
pub const STRIPE_PARTS: usize = 64;

/// How many rows the writer wants to see before it decides whether a varchar column gets to keep
/// its global dictionary.
///
/// See [`prepare::drops_dictionary`]. A stripe is up to [`STRIPE_PARTS`] parts, so most tables give it
/// far more than this and it binds only on a table that is smaller than one stripe. A handful of
/// rows says nothing about whether a column repeats itself, and the answer that costs nothing when
/// the sample is that small is the one the writer has always given, which is to keep the dictionary.
const DICTIONARY_DECIDE_ROWS: usize = 4_096;

/// Out of ten. A varchar column loses its dictionary when more than this many rows in ten of the
/// first stripe held a value that stripe had not seen before.
///
/// See [`prepare::drops_dictionary`]. Nine and not five, because the properties a dictionary buys are
/// worth keeping everywhere they are real. On ClickBench the widest string column is `Referer` at
/// 0.131 of its first stripe and every other one is below that, so nothing there is near this and
/// every one of them keeps its dictionary, which is what a group by on codes wants. On TPC-H
/// `o_comment` and `c_comment` are at 0.97 and are what this catches.
///
/// `l_comment` sits at 0.883 and so keeps its dictionary. Eight was built and measured rather than
/// argued about, and it is not a clear win: it takes `select l_comment from lineitem` from 4.335 G
/// instructions to 3.473 G and the file from 280.2 MB to 260.4 MB, and it takes a `like` over the
/// same column from 3.29 G to 4.27 G, because a dictionary runs the predicate once a distinct value
/// and there are 3.6 M of those to 6.0 M rows. The 22 query suite came out 6.91 s against 7.07 s in
/// favour of nine. So nine stays until there is a reason to prefer one of those shapes. See #1137.
const DICTIONARY_DISTINCT_IN_TEN: usize = 9;

/// Bytes one part takes in a stripe's index page: four for the length, eight for the checksum.
const INDEX_ENTRY: usize = size_of::<u32>() + size_of::<u64>();

/// Bytes one column's section of a stripe's index page takes, including its own trailing checksum.
fn index_section(parts: usize) -> Result<usize> {
    parts
        .checked_mul(INDEX_ENTRY)
        .and_then(|bytes| bytes.checked_add(size_of::<u64>()))
        .ok_or_else(|| invalid("index page length overflow"))
}

impl Writer {
    /// Opens a committed file and starts a table in the generation after the one it holds.
    ///
    /// The tables already in the file are carried forward by name and by directory pointer, and
    /// their pages are not read. Nothing in the file is overwritten: the new table's pages and the
    /// new catalog go on the end, past the catalog the committed generation points at, and the one
    /// write that is not an append is the slot in the header that [`Writer::finish`] does last.
    ///
    /// That slot is the other one. A file committed at generation 1 is named by the slot at 16 and
    /// generation 2 writes the one at 44, so until the last four bytes of the commit land the file
    /// still reads as the generation before it, and a slot torn across a write fails its checksum
    /// and the reader falls back to the one beside it. This is what the second slot has always been
    /// for.
    ///
    /// # Errors
    ///
    /// If the file has no valid committed directory, is not this build's format, repeats the name
    /// of a table already in it that holds rows, has a field with no scalar encoding, or cannot be
    /// written.
    pub fn open(
        path: impl AsRef<Path>,
        name: impl Into<String>,
        fields: Vec<Field>,
    ) -> Result<Self> {
        Self::open_in(&RealFilesystem::new(), path, name, fields)
    }

    /// [`Writer::open`] on a file in `fs`, which is how a crash test runs an append against the
    /// simulated filesystem.
    ///
    /// # Errors
    ///
    /// The same as [`Writer::open`].
    pub fn open_in(
        fs: &dyn Filesystem,
        path: impl AsRef<Path>,
        name: impl Into<String>,
        fields: Vec<Field>,
    ) -> Result<Self> {
        for field in &fields {
            type_tag(&field.ty)?;
        }
        let name = name.into();
        let file = fs.open(path.as_ref(), OpenMode::ReadWrite)?;
        let size = file.len()?;
        let (slot, bytes, _) = committed_slot(&*file, size)?;
        let (mut closed, views) = decode_catalog(&bytes, size)?;
        // A table already in the file under this name is only in the way if it holds rows. One that
        // holds none has no pages for this generation to carry and no reader that could lose
        // anything, so the table being started here takes its place in the catalog rather than
        // colliding with it, and `finish` writes the new entry where the old one was.
        //
        // That is not a corner. It is the shape every loading script writes: the schema goes in one
        // statement and the rows go in the next, and a checkpoint between them commits the empty
        // table. Before this, the second statement had to build the whole table in memory because
        // the first had already put the name in the file, which is how a load of a table larger
        // than memory became a load that needed memory the size of the table.
        if let Some(at) = closed.iter().position(|held| held.name == name) {
            if closed[at].rows > 0 {
                return Err(invalid("two tables in one native file have the same name"));
            }
            closed.remove(at);
        }
        // The generation of the slot whose bytes checksummed, and not the highest number in the
        // header. A slot torn across a write can hold any number at all, and taking that one would
        // be choosing which slot to overwrite from a value nothing has vouched for, which is how a
        // half written commit gets to destroy the one good copy beside it.
        let generation = slot
            .generation
            .checked_add(1)
            .ok_or_else(|| invalid("native file generation overflow"))?;
        Ok(Self {
            file,
            // The end of the file, so that the committed generation's catalog stays where its slot
            // says it is and keeps naming a file a reader can still open.
            at: size,
            written_back: size,
            dictionaries: fields
                .iter()
                .map(|field| coded_type(&field.ty).then(GlobalDictionary::new))
                .collect(),
            coded: Arc::new(prepare::Coding::new(fields.iter().map(|field| coded_type(&field.ty)))),
            gathers: fields.iter().map(|field| stats::Gather::new(&field.ty, generation)).collect(),
            lent: None,
            table: Table {
                name,
                dictionaries: vec![None; fields.len()],
                dictionary_payloads: Vec::new(),
                demoted: Vec::new(),
                distincts: vec![None; fields.len()],
                fields,
                stripes: Vec::new(),
                rows: 0,
                frequencies: Vec::new(),
                pair_frequencies: Vec::new(),
                frequency_texts: Vec::new(),
                host_groups: None,
                clustering: None,
                generation,
                sections: Vec::new(),
            },
            generation,
            order: Vec::new(),
            next_order: 0,
            pending: Vec::with_capacity(STRIPE_PARTS),
            closed,
            views,
            profile: None,
        })
    }

    /// Creates a new v10 file and its first table.
    ///
    /// # Errors
    ///
    /// If the file exists, a field has no scalar encoding, or the path cannot be written.
    pub fn create(
        path: impl AsRef<Path>,
        name: impl Into<String>,
        fields: Vec<Field>,
    ) -> Result<Self> {
        Self::create_in(&RealFilesystem::new(), path, name, fields)
    }

    /// [`Writer::create`] with the file made in `fs` rather than on the real filesystem.
    ///
    /// Every call the writer makes on the file from here to [`Writer::finish`] goes to that
    /// filesystem, which is what lets a test built on `rudb_io::SimFilesystem` stop a load at any
    /// one of them and look at what a crash there would leave on the disk.
    ///
    /// # Errors
    ///
    /// The same as [`Writer::create`].
    pub fn create_in(
        fs: &dyn Filesystem,
        path: impl AsRef<Path>,
        name: impl Into<String>,
        fields: Vec<Field>,
    ) -> Result<Self> {
        for field in &fields {
            type_tag(&field.ty)?;
        }
        let file = fs.open(path.as_ref(), OpenMode::CreateNew)?;
        let mut header = [0; HEADER as usize];
        header[..8].copy_from_slice(MAGIC);
        header[8..12].copy_from_slice(&FORMAT.to_le_bytes());
        file.write_at(0, &header)?;
        Ok(Self {
            file,
            at: HEADER,
            written_back: HEADER,
            dictionaries: fields
                .iter()
                .map(|field| coded_type(&field.ty).then(GlobalDictionary::new))
                .collect(),
            coded: Arc::new(prepare::Coding::new(fields.iter().map(|field| coded_type(&field.ty)))),
            gathers: fields.iter().map(|field| stats::Gather::new(&field.ty, 1)).collect(),
            lent: None,
            table: Table {
                name: name.into(),
                dictionaries: vec![None; fields.len()],
                dictionary_payloads: Vec::new(),
                demoted: Vec::new(),
                distincts: vec![None; fields.len()],
                fields,
                stripes: Vec::new(),
                rows: 0,
                frequencies: Vec::new(),
                pair_frequencies: Vec::new(),
                frequency_texts: Vec::new(),
                host_groups: None,
                clustering: None,
                generation: 1,
                sections: Vec::new(),
            },
            generation: 1,
            order: Vec::new(),
            next_order: 0,
            pending: Vec::with_capacity(STRIPE_PARTS),
            closed: Vec::new(),
            views: Vec::new(),
            profile: None,
        })
    }

    /// Creates a new file that holds no table at all, committed and ready to open.
    ///
    /// A database somebody dropped the last table out of is still a database, and until this there
    /// was no way to write one down. Every other way into this file goes through a table, because
    /// [`Writer::create`] takes the first one and [`Writer::finish`] commits the one it is on, so a
    /// catalog with nothing in it could be read and not written. The format already allowed it: the
    /// catalog is a count and that many entries, and a count of nought encodes and decodes the same
    /// way every other count does, which is why nothing here is a version change.
    ///
    /// It hands back nothing rather than a writer, because a writer with no table is a writer with
    /// nothing to append to. A file that is going to hold a table is [`Writer::create`], and one
    /// that is going to have a table added to it later is [`Writer::open`], which reads what this
    /// wrote the same way it reads any other generation.
    ///
    /// It takes the views anyway, because a database with no table can still have views in it. A
    /// view over `range` or over another view names no table, so dropping the last table out of a
    /// database does not have to leave the catalog with nothing worth writing down.
    ///
    /// # Errors
    ///
    /// If the file exists or the path cannot be written.
    pub fn empty(path: impl AsRef<Path>, views: &[ViewEntry]) -> Result<()> {
        let file = RealFilesystem::new().open(path.as_ref(), OpenMode::CreateNew)?;
        let mut header = [0; HEADER as usize];
        header[..8].copy_from_slice(MAGIC);
        header[8..12].copy_from_slice(&FORMAT.to_le_bytes());
        file.write_at(0, &header)?;
        let catalog = encode_catalog(&[], views)?;
        file.write_at(HEADER, &catalog)?;
        // The same two syncs in the same order as [`Writer::finish`], and for the same reason. The
        // catalog is on the disk before the slot names it, so a file this is interrupted in the
        // middle of is a header with no valid slot rather than a slot pointing at nothing.
        file.sync()?;
        let slot = Slot {
            offset: HEADER,
            length: u32::try_from(catalog.len()).map_err(|_| invalid("catalog length overflow"))?,
            generation: 1,
            hash: checksum(&catalog),
        };
        file.write_at(slot_offset(1), &slot.bytes())?;
        file.sync()?;
        Ok(())
    }

    /// Closes the table this writer is on and starts another one in the same file.
    ///
    /// Nothing is published here. The closed table's directory is written so that the bytes are on
    /// disk and its span is known, and the catalog that names it is only written by
    /// [`Writer::finish`], so a crash between two tables leaves the previous generation intact.
    ///
    /// # Errors
    ///
    /// If the name repeats a table already closed, a field has no scalar encoding, or the table
    /// being closed cannot be written.
    pub fn next(mut self, name: impl Into<String>, fields: Vec<Field>) -> Result<Self> {
        for field in &fields {
            type_tag(&field.ty)?;
        }
        let name = name.into();
        let entry = self.close()?;
        if self.closed.iter().chain(std::iter::once(&entry)).any(|held| held.name == name) {
            return Err(invalid("two tables in one native file have the same name"));
        }
        let Self { file, at, generation, mut closed, views, .. } = self;
        closed.push(entry);
        Ok(Self {
            file,
            written_back: at,
            at,
            generation,
            closed,
            views,
            profile: None,
            dictionaries: fields
                .iter()
                .map(|field| coded_type(&field.ty).then(GlobalDictionary::new))
                .collect(),
            coded: Arc::new(prepare::Coding::new(fields.iter().map(|field| coded_type(&field.ty)))),
            gathers: fields.iter().map(|field| stats::Gather::new(&field.ty, generation)).collect(),
            lent: None,
            table: Table {
                name,
                dictionaries: vec![None; fields.len()],
                dictionary_payloads: Vec::new(),
                demoted: Vec::new(),
                distincts: vec![None; fields.len()],
                fields,
                stripes: Vec::new(),
                rows: 0,
                frequencies: Vec::new(),
                pair_frequencies: Vec::new(),
                frequency_texts: Vec::new(),
                host_groups: None,
                clustering: None,
                generation,
                sections: Vec::new(),
            },
            order: Vec::new(),
            next_order: 0,
            pending: Vec::with_capacity(STRIPE_PARTS),
        })
    }

    /// Sets the views the next commit writes down, replacing whatever was carried forward.
    ///
    /// It replaces rather than adds because the caller has the whole catalog in front of it and the
    /// writer does not. A view that was dropped is a view that is not in the list any more, and
    /// there is no other way for the writer to hear about that, since nothing else it is told about
    /// mentions views at all.
    ///
    /// A writer that is never told anything writes back the views it read at [`Writer::open`], so a
    /// checkpoint that only had a table to append does not quietly drop them.
    #[must_use]
    pub fn with_views(mut self, views: Vec<ViewEntry>) -> Self {
        self.views = views;
        self
    }

    /// Charges the stages this writer runs to `profile`.
    ///
    /// For the table being written now. [`Writer::next`] starts the next table without one,
    /// because a second table's stripes charged to the first table's load would be a profile of
    /// neither.
    #[must_use]
    pub fn with_profile(mut self, profile: Arc<LoadProfile>) -> Self {
        self.profile = Some(profile);
        self
    }

    /// Sets what the table's global dictionaries may hold between them before the one growing
    /// fastest stops taking values, which is [`DICTIONARY_CAP_BYTES`] unless this says
    /// otherwise. It applies to every [`Preparer`] and [`Merger`] this writer has handed out too.
    #[must_use]
    pub fn with_dictionary_cap(self, bytes: u64) -> Self {
        self.coded.cap(bytes);
        self
    }

    /// Records the order this table's rows are meant to be stored in.
    ///
    /// The declaration goes in the table directory and comes back out of
    /// [`Table::clustering`]. Nothing here sorts anything, and nothing here checks that the rows
    /// handed to [`Writer::append`] arrive in the order this claims. That is deliberate for now:
    /// the thing that was missing was a place to write the order down, and a loader that honours
    /// the declaration is the next piece rather than this one.
    ///
    /// The declaration applies to the table the writer is currently on, so it is set after
    /// [`Writer::next`] rather than once for the file.
    ///
    /// # Errors
    ///
    /// If the declaration names a column this table does not have.
    pub fn declare(mut self, clustering: Clustering) -> Result<Self> {
        // Rebuilt against this table's own column count rather than trusted, because the caller
        // built it against a catalog entry and the two could have drifted.
        self.table.clustering = Some(Clustering::new(
            clustering.columns().to_vec(),
            clustering.width(),
            &self.table.fields,
        )?);
        Ok(self)
    }

    /// Appends bytes at the end of the file and moves the writer's own offset past them.
    ///
    /// Every write in here goes through this, so that [`Writer::at`] is the only answer to where
    /// anything is and the file's cursor is never consulted for it.
    fn put(&mut self, bytes: &[u8]) -> Result<()> {
        self.file.write_at(self.at, bytes)?;
        self.at = self
            .at
            .checked_add(bytes.len() as u64)
            .ok_or_else(|| invalid("native file length overflow"))?;
        if self.at - self.written_back >= WRITEBACK_STRETCH {
            self.file.start_writeback(self.written_back, self.at - self.written_back);
            self.written_back = self.at;
        }
        Ok(())
    }

    /// Writes one chunk as independently readable column pages.
    ///
    /// # Errors
    ///
    /// If its width or types differ from the declared table, or a page exceeds its bound.
    pub fn append(&mut self, chunk: &Chunk) -> Result<()> {
        let order = (self.next_order, 0);
        self.next_order = self.next_order.saturating_add(1);
        self.append_at(order, chunk)
    }

    /// Writes one chunk and records its source position for directory ordering.
    ///
    /// Pages may be encoded by parallel pipeline instances and reach the file in completion order.
    /// The stripe they land in is sorted by this key at commit, and [`Self::finish`] rejects a
    /// sequence whose parts do not come out in source order once the stripes are sorted, because a
    /// stripe groups whatever arrived together and cannot put a late part back where it belongs.
    ///
    /// # Errors
    ///
    /// The same as [`Self::append`].
    pub fn append_at(&mut self, order: (u64, u64), chunk: &Chunk) -> Result<()> {
        if chunk.is_empty() {
            return Ok(());
        }
        self.admit(chunk)?;
        if self.pending.last().is_some_and(|last| last.order > order) {
            self.flush_pending()?;
        }
        // Cloned rather than encoded, and a clone of a chunk that owns its buffers is a copy of
        // them. Sixty four parts of a hundred and five columns is tens of megabytes held for the
        // length of a stripe and a few seconds of memory traffic over a whole ClickBench load,
        // against the hundreds of seconds of encode this is what lets off one thread.
        self.pending.push(PendingChunk { order, chunk: chunk.clone() });
        if self.pending.len() == STRIPE_PARTS {
            self.flush_pending()?;
        }
        Ok(())
    }

    /// Writes a run of chunks as one stripe of its own.
    ///
    /// [`Self::append_at`] decides where a stripe ends by watching the orders go past, which works
    /// when one caller hands over every chunk in source order and does not when several do. A
    /// writer being fed by more than one pipeline instance sees the orders interleave, and a stripe
    /// that ends every time two of them cross is a stripe of one or two parts.
    ///
    /// So the grouping moves to the caller. Whoever is buffering hands over a run it already knows
    /// is contiguous and in order, and gets a stripe holding exactly that run. The orders still
    /// have to come out in source order once the stripes are sorted, which [`Self::finish`] checks,
    /// so the runs from different callers may interleave with each other but may not overlap.
    ///
    /// # Errors
    ///
    /// The same as [`Self::append`], and if the run is longer than [`STRIPE_PARTS`].
    pub fn append_stripe(&mut self, parts: Vec<((u64, u64), Chunk)>) -> Result<()> {
        if parts.len() > STRIPE_PARTS {
            return Err(invalid("a stripe was handed more parts than it holds"));
        }
        // Whatever an earlier caller left behind is its own stripe rather than the front of this
        // one, because the two runs are from different places in the source and a stripe is a run.
        self.flush_pending()?;
        for (order, chunk) in parts {
            if chunk.is_empty() {
                continue;
            }
            self.admit(&chunk)?;
            self.pending.push(PendingChunk { order, chunk });
        }
        self.flush_pending()
    }

    /// Checks a chunk against the declared table and counts its rows in.
    fn admit(&mut self, chunk: &Chunk) -> Result<()> {
        if chunk.width() != self.table.fields.len() {
            return Err(invalid("chunk width differs from table schema"));
        }
        for (index, field) in self.table.fields.iter().enumerate() {
            if chunk.column(index)?.logical_type() != &field.ty {
                return Err(invalid("chunk type differs from table schema"));
            }
        }
        self.table.rows = self
            .table
            .rows
            .checked_add(chunk.len())
            .ok_or_else(|| invalid("row count overflow"))?;
        Ok(())
    }

    /// One column's parts of a stripe as pages, for a column with no global dictionary.
    fn encode_pages(columns: &[&Vector]) -> Result<ColumnStripe> {
        let mut stripe = ColumnStripe {
            pages: Vec::with_capacity(columns.len()),
            codes: Vec::with_capacity(columns.len()),
            sieves: Vec::with_capacity(columns.len()),
            ranges: Vec::with_capacity(columns.len()),
        };
        let mut settling = Settling::default();
        for &column in columns {
            Self::encode_page(&mut stripe, &mut settling, column)?;
        }
        Ok(stripe)
    }

    /// One more part of a column with no global dictionary as a page, after the ones already in
    /// `stripe`. The parts have to come in order, since `settling` carries from one to the next.
    fn encode_page(
        stripe: &mut ColumnStripe,
        settling: &mut Settling,
        column: &Vector,
    ) -> Result<()> {
        let bytes = encode(column, settling)?;
        if bytes.len() > MAX_PAGE {
            return Err(invalid("column page exceeds the configured bound"));
        }
        // The range is built first because the sieve reads it rather than walking the column a
        // second time to find out how wide it is.
        let range = Range::of(column);
        // A sieve at least as large as the part it indexes is not written. A reader reads the
        // sieve to decide whether to read the part, so when the sieve is the larger of the two
        // it has already spent more than the read it is trying to avoid, and that holds even if
        // it rejects every time. It is a necessary condition rather than the whole rule, which
        // is that a sieve pays when its bytes are under the rejection rate times the part's,
        // but the rejection rate depends on what a query probes for and the writer does not
        // know that. The necessary half needs two numbers that are both in hand here.
        //
        // A column with a global dictionary gets none, because it already has an exact
        // membership index per stripe. Those do not come through here. See [`prepare`].
        let sieve =
            Sieve::of(column, &range, SIEVE_BUDGET).filter(|sieve| sieve.len() < bytes.len());
        stripe.pages.push(bytes);
        stripe.codes.push(None);
        stripe.sieves.push(sieve);
        stripe.ranges.push(range);
        Ok(())
    }

    /// Writes every encoded dictionary block that is not in the file yet and forgets its bytes.
    ///
    /// This is what keeps a load from holding its dictionaries' payload. The blocks land between
    /// stripes wherever the writer is, which is fine because the index says where each one is.
    fn place_blocks(&mut self) -> Result<()> {
        if let Some(lent) = self.lent.clone() {
            return self.place_lent_blocks(&lent);
        }
        let mut dictionaries = std::mem::take(&mut self.dictionaries);
        let placed = dictionaries.iter_mut().flatten().try_for_each(|dictionary| {
            for block in std::mem::take(&mut dictionary.blocks) {
                let start = self.at;
                self.put(&block)?;
                dictionary.placed.push(Placed {
                    start,
                    length: block.len() as u64,
                    hash: checksum(&block),
                });
            }
            Ok(())
        });
        self.dictionaries = dictionaries;
        placed
    }

    /// [`Writer::place_blocks`] while a [`Merger`] has the dictionaries.
    ///
    /// A column whose merge is running is passed over rather than waited for, because the writer's
    /// lock is held here and a merge of `URL` can take tens of milliseconds. Its blocks go out with
    /// a later stripe, or at the close.
    fn place_lent_blocks(&mut self, lent: &Lent) -> Result<()> {
        for column in lent.columns() {
            let Ok(mut held) = column.try_lock() else { continue };
            let Some(dictionary) = held.dictionary.as_mut() else { continue };
            for block in std::mem::take(&mut dictionary.blocks) {
                let start = self.at;
                self.put(&block)?;
                dictionary.placed.push(Placed {
                    start,
                    length: block.len() as u64,
                    hash: checksum(&block),
                });
            }
        }
        Ok(())
    }

    /// Takes the dictionaries and the statistics back from the [`Merger`] that has them.
    ///
    /// A merge that starts after this is refused, since whatever it merged would be lost.
    fn reclaim(&mut self) -> Result<()> {
        let Some(lent) = self.lent.take() else { return Ok(()) };
        let (dictionaries, gathers) = lent.reclaim()?;
        self.dictionaries = dictionaries;
        self.gathers = gathers;
        Ok(())
    }

    /// Writes the buffered parts as one stripe, each column's parts contiguous on disk.
    ///
    /// The same four steps a caller holding this writer behind a lock takes, with nobody else
    /// waiting between them. See [`prepare`].
    fn flush_pending(&mut self) -> Result<()> {
        if self.pending.is_empty() {
            return Ok(());
        }
        let held = std::mem::take(&mut self.pending);
        let prepared = self.preparer().prepare_held(held)?;
        let merged = self.merge_held(prepared)?;
        let paged = merged.pages()?;
        self.write_paged(paged)
    }

    /// Writes one stripe whose pages are built, each column's parts contiguous on disk.
    fn write_stripe(&mut self, held: &[Part], encoded: Vec<ColumnStripe>) -> Result<()> {
        let width = self.table.fields.len();
        let parts = held.len();
        if encoded.len() != width {
            return Err(Error::internal("a stripe came to the writer with the wrong columns"));
        }
        let profile = self.profile.clone();
        if let Some(profile) = &profile {
            let rows = held.iter().map(|part| part.rows as u64).sum();
            let raw = held.iter().map(|part| part.footprint as u64).sum();
            let pages =
                encoded.iter().flat_map(|stripe| &stripe.pages).map(|page| page.len() as u64).sum();
            profile.moved(Stage::Pages, raw, pages, rows);
        }
        // Before a byte of the stripe is written, so that the blocks the stripe's pages were built
        // with, and any that were waiting on them, are let go of now rather than a stripe later.
        let timing = profile.as_deref().map(|profile| profile.span(Stage::Dictionary));
        let before = self.at;
        self.place_blocks()?;
        drop(timing);
        if let Some(profile) = &profile {
            profile.moved(Stage::Dictionary, 0, self.at - before, 0);
        }
        let timing = profile.as_deref().map(|profile| profile.span(Stage::Write));
        let before = self.at;
        let mut pages = Vec::with_capacity(width);
        let mut memberships = vec![None; width];
        let mut ranges = Vec::with_capacity(width);
        let mut index = Vec::with_capacity(width.saturating_mul(index_section(parts)?));
        for stripe in &encoded {
            let offset = self.at;
            let section = index.len();
            let mut length = 0_usize;
            for bytes in &stripe.pages {
                self.file.write_at(self.at + length as u64, bytes)?;
                put_u32(
                    &mut index,
                    u32::try_from(bytes.len()).map_err(|_| invalid("part length overflow"))?,
                );
                put_u64(&mut index, checksum(bytes));
                length = length
                    .checked_add(bytes.len())
                    .ok_or_else(|| invalid("column page length overflow"))?;
            }
            let hash = checksum(&index[section..]);
            put_u64(&mut index, hash);
            if length > MAX_PAGE {
                return Err(invalid("column page exceeds the configured bound"));
            }
            self.at = self
                .at
                .checked_add(length as u64)
                .ok_or_else(|| invalid("native file length overflow"))?;
            pages.push(Span {
                offset,
                length: u32::try_from(length).map_err(|_| invalid("page length overflow"))?,
            });
            ranges.push(merged_range(stripe.ranges.iter().cloned()));
        }
        for (membership, stripe) in memberships.iter_mut().zip(&encoded) {
            if stripe.codes.iter().all(Option::is_none) {
                continue;
            }
            let lists = stripe
                .codes
                .iter()
                .map(|codes| codes.clone().unwrap_or_default())
                .collect::<Vec<_>>();
            let bytes = encode_membership(&merged_codes(lists));
            let offset = self.at;
            self.put(&bytes)?;
            *membership = Some(Page {
                offset,
                length: u32::try_from(bytes.len())
                    .map_err(|_| invalid("membership page length overflow"))?,
                hash: checksum(&bytes),
            });
        }
        let mut sieves = vec![None; width];
        for (page, stripe) in sieves.iter_mut().zip(&encoded) {
            if stripe.sieves.iter().all(Option::is_none) {
                continue;
            }
            let bytes = encode_sieves(stripe.sieves.iter())?;
            let offset = self.at;
            self.put(&bytes)?;
            *page = Some(Page {
                offset,
                length: u32::try_from(bytes.len())
                    .map_err(|_| invalid("sieve page length overflow"))?,
                hash: checksum(&bytes),
            });
        }
        // A stripe of one part has the same rows in it as that part, so its own bounds are already
        // the part's and a page here would say what the directory says. Everywhere else the page is
        // written unless it comes to more than the column it indexes, which is the rule the sieves
        // go by and for the same reason: a reader reads this to decide whether to read the column,
        // so a page larger than the column has spent more than the read it is avoiding.
        let mut part_ranges = vec![None; width];
        if parts > 1 {
            for ((page, stripe), span) in part_ranges.iter_mut().zip(&encoded).zip(&pages) {
                let bytes = encode_part_ranges(&stripe.ranges)?;
                if bytes.len() >= span.length as usize {
                    continue;
                }
                let offset = self.at;
                self.put(&bytes)?;
                *page = Some(Page {
                    offset,
                    length: u32::try_from(bytes.len())
                        .map_err(|_| invalid("part range page length overflow"))?,
                    hash: checksum(&bytes),
                });
            }
        }
        let offset = self.at;
        self.put(&index)?;
        let index = Span {
            offset,
            length: u32::try_from(index.len())
                .map_err(|_| invalid("index page length overflow"))?,
        };
        let mut rows = 0_usize;
        let mut lengths = Vec::with_capacity(parts);
        let mut span = None;
        for part in held {
            rows = rows.checked_add(part.rows).ok_or_else(|| invalid("row count overflow"))?;
            lengths.push(u32::try_from(part.rows).map_err(|_| invalid("part row count overflow"))?);
            span = Some(span.map_or((part.order, part.order), |(first, _)| (first, part.order)));
        }
        self.order.push(span.ok_or_else(|| invalid("a stripe was flushed with no parts"))?);
        self.table.stripes.push(Stripe {
            rows,
            parts: lengths,
            index,
            pages,
            memberships: Pages::from_slots(memberships)?,
            sieves: Pages::from_slots(sieves)?,
            part_ranges: Pages::from_slots(part_ranges)?,
            zone: Zone::from_ranges(ranges),
        });
        drop(timing);
        if let Some(profile) = &profile {
            profile.moved(Stage::Write, 0, self.at - before, rows as u64);
        }
        Ok(())
    }

    /// Finds exact heavy hitters without keeping a hash table for every numeric column while the
    /// load is live. The pages are already in the target file, so one column at a time uses a
    /// bounded Misra-Gries candidate table and then recounts only those candidates.
    ///
    /// The first of those passes also counts the column's distinct values exactly, up to the cap in
    /// [`distinct`], which is the number a string column gets from its dictionary. It comes back
    /// beside the summary because a column whose heavy hitters cannot be proved can still have been
    /// counted.
    ///
    /// The tables are keyed by a value's sixty four bits rather than by [`FrequencyValue`], and a
    /// null is counted beside them. Every integer type the format stores fits in those bits, so
    /// within one column two values share bits only if they are the same value, and a sixteen byte
    /// entry keeps the whole candidate table in the second level cache where the forty eight byte
    /// one did not. The null takes part in the candidate table exactly as a key would: it holds a
    /// place while its count is above zero, and it is decremented with the rest.
    ///
    /// `counted` is false for a column whose sketch says its distinct values are far past what the
    /// exact set holds. It still gets its frequencies, and a count only if it turns out to have
    /// fewer values than the candidate table, which is the count that costs nothing.
    fn numeric_frequency(
        &self,
        column: usize,
        counted: bool,
    ) -> Result<(Option<FrequencySummary>, Option<u64>)> {
        let signed = match self.table.fields[column].ty {
            LogicalType::TinyInt
            | LogicalType::SmallInt
            | LogicalType::Integer
            | LogicalType::BigInt
            | LogicalType::Date
            | LogicalType::Timestamp => true,
            LogicalType::UTinyInt
            | LogicalType::USmallInt
            | LogicalType::UInteger
            | LogicalType::UBigInt => false,
            _ => return Ok((None, None)),
        };
        let value_of = |bits: Option<u64>| match bits {
            None => FrequencyValue::Null,
            Some(bits) => integer_value(bits, signed),
        };
        // A column the writer's tally held whole has its exact counts already, gathered as the rows
        // went past, so the pages are not read back to count them again. On `hits` that is most of
        // the flag and enum columns. The tally only speaks for the whole column when it saw every
        // row, which is the same check the statistics make before they are written.
        let tallied = self
            .gathers
            .get(column)
            .and_then(Option::as_ref)
            .filter(|gather| gather.rows() == self.table.rows as u64)
            .and_then(stats::Gather::frequencies)
            .and_then(|(values, nulls)| {
                let entries = values
                    .iter()
                    .map(|(value, count)| {
                        let value = value_of(Some(frequency_bits(value)?));
                        Some(FrequencyEntry { value, count: *count })
                    })
                    .chain((nulls != 0).then_some(Some(FrequencyEntry {
                        value: FrequencyValue::Null,
                        count: nulls,
                    })))
                    .collect::<Option<Vec<_>>>()?;
                Some((entries, values.len() as u64))
            });
        // A column the sketch expects to fit the exact set is counted there, every value with the
        // rows holding it, which is its distinct count and its frequencies from one read of its
        // pages. Only a column past the set's cap goes through the candidate table.
        let exact = match (&tallied, counted) {
            (None, true) => self.exact_frequency(column, signed)?,
            _ => None,
        };
        let (mut entries, decrements, distinct_count) = match (tallied, exact) {
            (Some((entries, distinct)), _) => (entries, 0, Some(distinct)),
            (None, Some((Some(entries), distinct))) => (entries, 0, Some(distinct)),
            (None, Some((None, distinct))) => return Ok((None, Some(distinct))),
            (None, None) => {
                // Rows arrive a run of equal values at a time, because a sorted column is runs and
                // a flag column is mostly one value, so a run is counted and inserted once rather
                // than per row.
                let mut first = Candidates::default();
                let mut run = Run::default();
                self.visit_numeric(column, signed, |_, bits| {
                    if let Some((ended, times)) = run.push(bits) {
                        first.add(ended, times);
                    }
                })?;
                if let Some((bits, times)) = run.take() {
                    first.add(bits, times);
                }
                // Until a candidate is turned away the table holds every value the column has, so
                // its size is the count.
                let (nulls, decrements) = (first.nulls, first.decrements);
                let distinct_count = (decrements == 0).then_some(first.held as u64);
                let (exact, null_count) = if decrements == 0 {
                    let exact = first
                        .pairs()
                        .map(|(bits, count)| (bits, u64::from(count)))
                        .collect::<FrequencyMap<_>>();
                    (exact, (nulls != 0).then_some(u64::from(nulls)))
                } else {
                    let mut lower = first.pairs().map(|(_, count)| count).collect::<Vec<_>>();
                    if nulls != 0 {
                        lower.push(nulls);
                    }
                    lower.sort_unstable_by(|left, right| right.cmp(left));
                    if lower.len() < FREQUENCY_BUILD_RANK
                        || u64::from(lower[FREQUENCY_BUILD_RANK - 1]) <= decrements
                    {
                        return Ok((None, distinct_count));
                    }
                    // Counted beside the slot each candidate sits in, since the table is not
                    // changed again and a lookup in it is the one probe the first pass made.
                    let mut recounts = vec![0_u64; first.slots.len()];
                    let mut null_count = (nulls != 0).then_some(0_u64);
                    let mut recount = |bits: Option<u64>, times: u32| {
                        let held = match bits {
                            Some(bits) => first.position(bits).map(|at| &mut recounts[at]),
                            None => null_count.as_mut(),
                        };
                        if let Some(count) = held {
                            *count = count.saturating_add(u64::from(times));
                        }
                    };
                    let mut run = Run::default();
                    self.visit_numeric(column, signed, |_, bits| {
                        if let Some((bits, times)) = run.push(bits) {
                            recount(bits, times);
                        }
                    })?;
                    if let Some((bits, times)) = run.take() {
                        recount(bits, times);
                    }
                    let exact = first
                        .slots
                        .iter()
                        .zip(&recounts)
                        .filter(|(slot, _)| slot.count != 0)
                        .map(|(slot, &count)| (slot.bits, count))
                        .collect::<FrequencyMap<_>>();
                    (exact, null_count)
                };
                let entries = exact
                    .into_iter()
                    .map(|(bits, count)| FrequencyEntry { value: value_of(Some(bits)), count })
                    .chain(
                        null_count
                            .map(|count| FrequencyEntry { value: FrequencyValue::Null, count }),
                    )
                    .collect::<Vec<_>>();
                (entries, decrements, distinct_count)
            }
        };
        let mut omitted_max = keep_most_frequent(&mut entries).max(decrements);
        // A complete value-to-count table is also the result of grouping this column.
        // Keep up to two leading frequencies for selectivity and equality predicates,
        // but leave multi-value grouped counts to the encoded rows at query time.
        if omitted_max == 0 && entries.len() > 1 {
            let retained = entries.len().saturating_sub(1).min(2);
            omitted_max = entries[retained].count;
            entries.truncate(retained);
        }
        let kept_rows = entries.iter().try_fold(0_u64, |total, entry| {
            total.checked_add(entry.count).filter(|&total| total <= FREQUENCY_ORDINALS as u64)
        });
        let mut ordinals = Vec::new();
        let mut ordinal_entries = Vec::new();
        if let Some(kept_rows) = kept_rows {
            let mut kept = FrequencyMap::default();
            let mut null_kept = None;
            for (at, entry) in entries.iter().enumerate() {
                let at = u16::try_from(at)
                    .map_err(|_| invalid("too many retained frequency entries"))?;
                match entry.value {
                    FrequencyValue::Integer(value) => {
                        kept.insert(value as u64, at);
                    }
                    FrequencyValue::Null => null_kept = Some(at),
                    FrequencyValue::Code(_) => {}
                }
            }
            ordinals.reserve(usize::try_from(kept_rows).unwrap_or(FREQUENCY_ORDINALS));
            ordinal_entries.reserve(usize::try_from(kept_rows).unwrap_or(FREQUENCY_ORDINALS));
            self.visit_numeric(column, signed, |ordinal, bits| {
                let held = match bits {
                    Some(bits) => kept.get(&bits).copied(),
                    None => null_kept,
                };
                if let Some(entry) = held {
                    ordinals.push(ordinal);
                    ordinal_entries.push(entry);
                }
            })?;
        }
        Ok((
            Some(FrequencySummary { entries, omitted_max, ordinals, ordinal_entries }),
            distinct_count,
        ))
    }

    /// Counts every value of an integer column and the rows holding it, and hands back the
    /// frequency entries worth keeping beside the distinct count, or nothing for a column with more
    /// values than [`distinct::ExactCounts`] keeps.
    ///
    /// The entries are `None` for a column with no value common enough to be worth a synopsis. The
    /// rule is the one the candidate table applied. A column with more values than that table holds
    /// keeps its frequencies only if its tenth commonest value is held by more rows than a
    /// Misra-Gries table of [`FREQUENCY_CANDIDATES`] could have decremented it by, which is its rows
    /// over one more than the candidates. The counts kept are exact either way, so the largest one
    /// left out is exact too and not the table's bound on it.
    ///
    /// Only the commonest entries and the ones tied with the first left out are built, since on a
    /// column of a million values the rest are thrown away the moment they are ranked.
    fn exact_frequency(
        &self,
        column: usize,
        signed: bool,
    ) -> Result<Option<(Option<Vec<FrequencyEntry>>, u64)>> {
        let mut set = distinct::ExactCounts::new();
        let mut nulls = 0_u64;
        let mut run = Run::default();
        let mut add = |bits: Option<u64>, times: u32| match bits {
            Some(bits) => set.insert(bits, times),
            None => nulls += u64::from(times),
        };
        self.visit_numeric(column, signed, |_, bits| {
            if let Some((bits, times)) = run.push(bits) {
                add(bits, times);
            }
        })?;
        if let Some((bits, times)) = run.take() {
            add(bits, times);
        }
        let Some(distinct) = set.count() else {
            return Ok(None);
        };
        // The commonest counts, one more than the entries kept so that the first left out is here.
        let mut top = std::collections::BinaryHeap::with_capacity(FREQUENCY_ENTRIES + 2);
        let mut rank = |count: u64| {
            if top.len() <= FREQUENCY_ENTRIES {
                top.push(Reverse(count));
            } else if top.peek().is_some_and(|&Reverse(least)| count > least) {
                top.pop();
                top.push(Reverse(count));
            }
        };
        set.visit(|_, count| rank(count));
        if nulls != 0 {
            rank(nulls);
        }
        let top = top.into_sorted_vec();
        let values = distinct + u64::from(nulls != 0);
        if values > FREQUENCY_CANDIDATES as u64 {
            let bound = self.table.rows as u64 / (FREQUENCY_CANDIDATES as u64 + 1);
            if top.get(FREQUENCY_BUILD_RANK - 1).is_none_or(|&Reverse(count)| count <= bound) {
                return Ok(Some((None, distinct)));
            }
        }
        let least = top.get(FREQUENCY_ENTRIES).map_or(0, |&Reverse(count)| count);
        let mut entries = Vec::with_capacity(FREQUENCY_ENTRIES + 1);
        set.visit(|bits, count| {
            if count >= least {
                entries.push(FrequencyEntry { value: integer_value(bits, signed), count });
            }
        });
        if nulls != 0 && nulls >= least {
            entries.push(FrequencyEntry { value: FrequencyValue::Null, count: nulls });
        }
        Ok(Some((Some(entries), distinct)))
    }

    /// Hands every row of an integer column to `visit` as its ordinal and its sixty four bits, or
    /// `None` for a null.
    ///
    /// `signed` says which of the two readings the column has. A packed unsigned column would come
    /// back from `signed_block` as a base plus a code in `i64`, which wraps for a value past the top
    /// of `BIGINT`, so only a signed column takes the block path.
    fn visit_numeric(
        &self,
        column: usize,
        signed: bool,
        mut visit: impl FnMut(u64, Option<u64>),
    ) -> Result<()> {
        let ty = &self.table.fields[column].ty;
        let mut start = 0_u64;
        let mut block = Vec::new();
        for stripe in &self.table.stripes {
            let spans = read_index(&self.file, stripe, column)?;
            let page = stripe.pages[column];
            let mut bytes = vec![0; page.length as usize];
            read_at(&self.file, page.offset, &mut bytes)?;
            for (span, &rows) in spans.iter().zip(&stripe.parts) {
                let part = part_bytes(&bytes, *span)?;
                if checksum(part) != span.hash {
                    return Err(invalid("column page checksum differs while building frequencies"));
                }
                let rows = rows as usize;
                let vector = decode(ty, rows, part, None)?;
                // Every signed layout a numeric column decodes to, which is every column of `hits`,
                // comes out as one run of `i64` and is walked as a slice. The row path below is for
                // the unsigned types and anything else that cannot be handed over that way.
                if signed && vector.signed_block(&mut block) && block.len() == rows {
                    if vector.none_null() {
                        for (row, &value) in block.iter().enumerate() {
                            visit(start.saturating_add(row as u64), Some(value as u64));
                        }
                    } else {
                        for (row, &value) in block.iter().enumerate() {
                            let bits = (!vector.is_null_at(row)).then_some(value as u64);
                            visit(start.saturating_add(row as u64), bits);
                        }
                    }
                    start = start.saturating_add(rows as u64);
                    continue;
                }
                // row at a time: frequency construction visits decoded values to update bounded candidates.
                for row in 0..rows {
                    let bits = if vector.is_null_at(row) {
                        None
                    } else {
                        // An unsigned column has no signed reading, and the documented fallback is
                        // the value itself. Every width the format stores fits in sixty four bits,
                        // so nothing is lost on the way through.
                        let widened = match vector.signed_at(row) {
                            Some(value) => Some(value as u64),
                            None => match vector.value_at(row) {
                                Value::UTinyInt(value) => Some(u64::from(value)),
                                Value::USmallInt(value) => Some(u64::from(value)),
                                Value::UInteger(value) => Some(u64::from(value)),
                                Value::UBigInt(value) => Some(value),
                                _ => None,
                            },
                        };
                        Some(widened.ok_or_else(|| {
                            invalid("numeric frequency page did not contain an integer value")
                        })?)
                    };
                    visit(start.saturating_add(row as u64), bits);
                }
                start = start.saturating_add(rows as u64);
            }
        }
        Ok(())
    }

    /// The columns that get numeric frequencies, which are the integer, date and timestamp ones.
    fn numeric_columns(&self) -> Vec<usize> {
        self.table
            .fields
            .iter()
            .enumerate()
            .filter_map(|(column, field)| {
                matches!(
                    field.ty,
                    LogicalType::TinyInt
                        | LogicalType::SmallInt
                        | LogicalType::Integer
                        | LogicalType::BigInt
                        | LogicalType::UTinyInt
                        | LogicalType::USmallInt
                        | LogicalType::UInteger
                        | LogicalType::UBigInt
                        | LogicalType::Date
                        | LogicalType::Timestamp
                )
                .then_some(column)
            })
            .collect()
    }

    /// Reads one stable dictionary code column only at sorted table-wide row ordinals.
    #[allow(dead_code)]
    fn stable_codes_at(&self, column: usize, ordinals: &[u64]) -> Result<Option<Vec<Option<u32>>>> {
        if self.dictionaries.get(column).and_then(Option::as_ref).is_none() {
            return Ok(None);
        }
        if ordinals.windows(2).any(|pair| pair[0] >= pair[1]) {
            return Err(invalid("frequency ordinals are not sorted and unique"));
        }
        let mut out = Vec::with_capacity(ordinals.len());
        let mut wanted = 0;
        let mut stripe_start = 0_u64;
        for stripe in &self.table.stripes {
            let stripe_end = stripe_start.saturating_add(stripe.rows as u64);
            if wanted == ordinals.len() || ordinals[wanted] >= stripe_end {
                stripe_start = stripe_end;
                continue;
            }
            let spans = read_index(&self.file, stripe, column)?;
            let page = stripe.pages[column];
            let mut bytes = vec![0; page.length as usize];
            read_at(&self.file, page.offset, &mut bytes)?;
            let mut part_start = stripe_start;
            for (span, &rows) in spans.iter().zip(&stripe.parts) {
                let part_end = part_start.saturating_add(u64::from(rows));
                if wanted < ordinals.len() && ordinals[wanted] < part_end {
                    let part = part_bytes(&bytes, *span)?;
                    if checksum(part) != span.hash {
                        return Err(invalid(
                            "column page checksum differs while building pair frequencies",
                        ));
                    }
                    let upto = ordinals.partition_point(|&ordinal| ordinal < part_end);
                    let positions = ordinals[wanted..upto]
                        .iter()
                        .map(|&ordinal| {
                            usize::try_from(ordinal.saturating_sub(part_start))
                                .map_err(|_| invalid("frequency row offset does not fit in memory"))
                        })
                        .collect::<Result<Vec<_>>>()?;
                    if !decode_selected_stable_codes(rows as usize, part, &positions, &mut out)? {
                        return Ok(None);
                    }
                    wanted = upto;
                }
                part_start = part_end;
            }
            stripe_start = stripe_end;
        }
        if wanted != ordinals.len() {
            return Err(invalid("frequency ordinal is outside the table"));
        }
        Ok(Some(out))
    }

    /// Derives bounded two-key leaders from numeric anchor ordinals and stable string codes.
    #[allow(dead_code)]
    fn pair_frequencies(
        &self,
        frequencies: &[Option<Frequencies>],
    ) -> Result<Vec<PairFrequencySummary>> {
        let anchors = frequencies
            .iter()
            .enumerate()
            .filter_map(|(column, summary)| {
                // A writer holds every synopsis it counted, so there is nothing stored to skip.
                match summary {
                    Some(Frequencies::Held(summary)) => Some(summary),
                    _ => None,
                }
                .filter(|summary| {
                    !summary.ordinals.is_empty()
                        && summary.ordinal_entries.len() == summary.ordinals.len()
                })
                .cloned()
                .map(|summary| (column, summary))
            })
            .collect::<Vec<_>>();
        let strings = self
            .dictionaries
            .iter()
            .enumerate()
            .filter_map(|(column, dictionary)| dictionary.as_ref().map(|_| column))
            .collect::<Vec<_>>();
        let mut summaries = Vec::new();
        for (first, anchors) in anchors {
            for &second in &strings {
                if summaries.len() == MAX_PAIR_FREQUENCIES {
                    return Ok(summaries);
                }
                let Some(codes) = self.stable_codes_at(second, &anchors.ordinals)? else {
                    continue;
                };
                if codes.len() != anchors.ordinal_entries.len() {
                    return Err(invalid("pair frequency columns have different lengths"));
                }
                let mut counts = HashMap::<(u16, Option<u32>), u64>::new();
                for (&anchor, code) in anchors.ordinal_entries.iter().zip(codes) {
                    *counts.entry((anchor, code)).or_default() += 1;
                }
                let mut entries = counts
                    .into_iter()
                    .map(|((first_entry, second), count)| PairFrequencyEntry {
                        first_entry,
                        second,
                        count,
                    })
                    .collect::<Vec<_>>();
                entries.sort_unstable_by(|left, right| {
                    right
                        .count
                        .cmp(&left.count)
                        .then_with(|| left.first_entry.cmp(&right.first_entry))
                        .then_with(|| left.second.cmp(&right.second))
                });
                let pair_omitted = entries.get(FREQUENCY_ENTRIES).map_or(0, |entry| entry.count);
                entries.truncate(FREQUENCY_ENTRIES);
                summaries.push(PairFrequencySummary {
                    first: u16::try_from(first)
                        .map_err(|_| invalid("pair frequency column index overflows"))?,
                    second: u16::try_from(second)
                        .map_err(|_| invalid("pair frequency column index overflows"))?,
                    entries,
                    omitted_max: anchors.omitted_max.max(pair_omitted),
                });
            }
        }
        Ok(summaries)
    }

    /// Writes the directory of the table this writer is on and says where it went.
    ///
    /// Everything [`Writer::finish`] used to do except the two writes that publish. Pulling it out
    /// is what lets a second table follow a first: the bytes of a closed table are complete and
    /// addressable while nothing yet points at them, and the pointer is the last write of the
    /// commit.
    ///
    /// # Errors
    ///
    /// If directory encoding or writing fails.
    fn close(&mut self) -> Result<Entry> {
        self.reclaim()?;
        self.flush_pending()?;
        // The rest of a table is its statistics, its dictionaries and its directory. The dictionary
        // work is charged as its own stage, because ranking a global dictionary can be most of what
        // this costs, and the rest as publish.
        let profile = self.profile.clone();
        let timing = profile.as_deref().map(|profile| profile.span(Stage::Publish));
        let before = self.at;
        let mut stripes = std::mem::take(&mut self.order)
            .into_iter()
            .zip(std::mem::take(&mut self.table.stripes))
            .collect::<Vec<_>>();
        stripes.sort_by_key(|(order, _)| order.0);
        let mut previous: Option<(u64, u64)> = None;
        for ((first, last), _) in &stripes {
            if previous.is_some_and(|previous| previous >= *first) {
                return Err(invalid("chunks did not arrive in source order"));
            }
            previous = Some(*last);
        }
        self.table.stripes = stripes.into_iter().map(|(_, stripe)| stripe).collect();
        drop(timing);
        let timing = profile.as_deref().map(|profile| profile.span(Stage::Dictionary));
        let placing = self.at;
        finish_dictionaries(&mut self.dictionaries)?;
        self.place_blocks()?;
        for dictionary in self.dictionaries.iter_mut().flatten() {
            dictionary.release_lookup();
            dictionary.recharge(profile.as_deref());
        }
        let (numeric, closed) = self.close_columns()?;
        let (frequencies, distincts): (Vec<Option<FrequencySummary>>, Vec<_>) =
            numeric.into_iter().unzip();
        let frequencies =
            frequencies.into_iter().map(|held| held.map(Frequencies::Held)).collect::<Vec<_>>();
        // Pair leaders are query results, not reusable column statistics.
        let pairs = Vec::new();
        self.table.frequencies = frequencies;
        self.table.distincts = distincts;
        self.table.pair_frequencies = pairs;
        if let Some(profile) = &profile {
            profile.release(self.dictionaries.iter().flatten().map(|held| held.charged).sum());
        }
        self.table.demoted = self
            .dictionaries
            .iter()
            .map(|dictionary| dictionary.as_ref().is_some_and(|held| held.demoted))
            .collect();
        if !self.table.demoted.contains(&true) {
            self.table.demoted = Vec::new();
        }
        self.dictionaries = Vec::new();
        self.table.dictionary_payloads = vec![0; self.table.fields.len()];
        self.table.frequency_texts = vec![Vec::new(); self.table.fields.len()];
        self.table.host_groups = None;
        for (index, closed) in closed.into_iter().enumerate() {
            let Some(closed) = closed else { continue };
            let ClosedDictionary { distinct, frequencies, texts, hosts, encoded, payload } = closed;
            self.table.distincts[index] = distinct;
            self.table.frequencies[index] = frequencies.map(Frequencies::Held);
            self.table.frequency_texts[index] = texts;
            if hosts.is_some() {
                self.table.host_groups = hosts;
            }
            let offset = self.at;
            self.put(&encoded.index)?;
            self.put(&encoded.ranks)?;
            self.put(&encoded.grams)?;
            self.table.dictionary_payloads[index] = payload;
            let length = encoded
                .index
                .len()
                .checked_add(encoded.ranks.len())
                .and_then(|len| len.checked_add(encoded.grams.len()))
                .ok_or_else(|| invalid("dictionary page length overflow"))?;
            self.table.dictionaries[index] = Some(Page {
                offset,
                length: u32::try_from(length)
                    .map_err(|_| invalid("dictionary page length overflow"))?,
                hash: checksum(&encoded.index),
            });
        }
        drop(timing);
        let timing = profile.as_deref().map(|profile| profile.span(Stage::Publish));
        let placed = self.at - placing;
        self.write_stats()?;
        let directory = encode_directory(&self.table)?;
        if directory.len() > MAX_DIRECTORY {
            return Err(invalid("directory exceeds the configured bound"));
        }
        let offset = self.at;
        self.put(&directory)?;
        drop(timing);
        if let Some(profile) = &profile {
            profile.moved(Stage::Dictionary, 0, placed, 0);
            profile.moved(Stage::Publish, 0, self.at - before - placed, 0);
        }
        Ok(Entry {
            name: self.table.name.clone(),
            fields: self.table.fields.clone(),
            rows: self.table.rows,
            nonzero: vec![None; self.table.fields.len()],
            aggregates: table_aggregate_sums(&self.table),
            distincts: self.table.distincts.clone(),
            extremes: table_integer_extremes(&self.table),
            frequencies: table_complete_numeric_frequencies(&self.table),
            directory: Page {
                offset,
                length: u32::try_from(directory.len())
                    .map_err(|_| invalid("directory length overflow"))?,
                hash: checksum(&directory),
            },
        })
    }

    /// Every numeric column's frequencies and every global dictionary's page and statistics, by
    /// column, as many columns at a time as [`CLOSE_BYTES`] allows.
    ///
    /// The two kinds read what is already written and write nothing, so they share one set of
    /// threads. Each was most of a second on `hits` with the other waiting for it, and neither keeps
    /// every core busy on its own. The most expensive column that fits is the one taken next, so
    /// the long ones start first and the short ones fill in behind them. A column that does not fit
    /// waits for one that is closing to finish, unless nothing is closing, in which case it goes
    /// alone.
    ///
    /// A numeric column is charged the exact distinct set its sketch says it will need, and one the
    /// sketch puts far past what that set can hold does not build it, because the set would fill,
    /// give up and have held 512 MiB for nothing. A column with no sketch is charged the whole set.
    /// Each job charges itself as its own span, publish for the numeric ones and dictionary for the
    /// rest, because it runs on a thread of its own and a span on this one would see the wall time
    /// and none of the CPU.
    #[allow(clippy::type_complexity)]
    fn close_columns(
        &self,
    ) -> Result<(Vec<(Option<FrequencySummary>, Option<u64>)>, Vec<Option<ClosedDictionary>>)> {
        let numeric = self.numeric_columns().into_iter().map(|column| {
            let estimate =
                self.gathers.get(column).and_then(Option::as_ref).and_then(stats::Gather::distinct);
            let counted = !estimate.is_some_and(distinct::beyond);
            let set =
                if counted { distinct::bytes_for(estimate.unwrap_or(f64::INFINITY)) } else { 0 };
            let cost = self.table.rows.saturating_mul(weight(&self.table.fields[column].ty));
            (Closing::Numeric { column, counted }, NUMERIC_CLOSE_BYTES + set, cost)
        });
        let dictionaries =
            self.dictionaries.iter().enumerate().filter_map(|(index, dictionary)| {
                let dictionary = dictionary.as_ref()?;
                let bytes = dictionary.closing_bytes();
                Some((Closing::Dictionary { index, dictionary }, bytes, bytes))
            });
        let mut jobs = numeric.chain(dictionaries).collect::<Vec<_>>();
        jobs.sort_by_key(|&(_, _, cost)| cost);
        let columns = self.table.fields.len();
        let mut frequencies = vec![(None, None); columns];
        let mut closed = (0..columns).map(|_| None).collect::<Vec<_>>();
        let profile = self.profile.as_deref();
        let run = |job: Closing<'_>, bytes: usize| -> Result<Closed> {
            let _holding = profile.map(|profile| profile.holding(bytes as u64));
            match job {
                Closing::Numeric { column, counted } => {
                    let _timing = profile.map(|profile| profile.span(Stage::Publish));
                    Ok(Closed::Numeric(column, self.numeric_frequency(column, counted)?))
                }
                Closing::Dictionary { index, dictionary } => {
                    let _timing = profile.map(|profile| profile.span(Stage::Dictionary));
                    Ok(Closed::Dictionary(index, self.close_dictionary(index, dictionary)?))
                }
            }
        };
        let workers = close_workers().min(jobs.len());
        let pieces = if workers <= 1 {
            jobs.into_iter().map(|(job, bytes, _)| run(job, bytes)).collect::<Result<Vec<_>>>()?
        } else {
            // The columns not taken yet, cheapest first, and the bytes the ones closing now hold.
            let state = Mutex::new((jobs, 0_usize));
            let finished = Condvar::new();
            std::thread::scope(|scope| {
                (0..workers)
                    .map(|_| {
                        scope.spawn(|| {
                            let mut mine = Vec::new();
                            loop {
                                let mut held = state.lock().map_err(|_| {
                                    Error::internal("a native close worker panicked")
                                })?;
                                let (job, bytes) = loop {
                                    let (jobs, busy) = &mut *held;
                                    if jobs.is_empty() {
                                        return Ok(mine);
                                    }
                                    let fits = jobs.iter().rposition(|&(_, bytes, _)| {
                                        *busy == 0 || busy.saturating_add(bytes) <= CLOSE_BYTES
                                    });
                                    if let Some(at) = fits {
                                        let (job, bytes, _) = jobs.remove(at);
                                        *busy += bytes;
                                        break (job, bytes);
                                    }
                                    held = finished.wait(held).map_err(|_| {
                                        Error::internal("a native close worker panicked")
                                    })?;
                                };
                                drop(held);
                                // Given back on the way out whether the close worked, failed or
                                // panicked, so that a worker waiting for room is never left waiting.
                                let _room = Room { state: &state, finished: &finished, bytes };
                                mine.push(run(job, bytes)?);
                            }
                        })
                    })
                    .collect::<Vec<_>>()
                    .into_iter()
                    .map(|handle| {
                        handle
                            .join()
                            .map_err(|_| Error::internal("a native close worker panicked"))?
                    })
                    .collect::<Result<Vec<_>>>()
            })?
            .into_iter()
            .flatten()
            .collect()
        };
        for piece in pieces {
            match piece {
                Closed::Numeric(column, summary) => frequencies[column] = summary,
                Closed::Dictionary(index, one) => closed[index] = Some(one),
            }
        }
        Ok((frequencies, closed))
    }

    /// One global dictionary's page and statistics, built from what is already in the file.
    ///
    /// Nothing is written here, so that [`Self::close`] can run this beside the numeric frequencies
    /// and put the pages down afterwards in column order, which is where they always went. The
    /// column's values are decoded in here and dropped before it returns, and
    /// [`Self::close_columns`] decides how many columns are in here at once.
    fn close_dictionary(
        &self,
        _index: usize,
        dictionary: &GlobalDictionary,
    ) -> Result<ClosedDictionary> {
        let (order, flat, bases) = dictionary.ranked_with_values(Some(&*self.file))?;
        // A code nothing counted is a code no non-null row of this column holds, which is the
        // empty string a null was written as and nothing else, because a code is only ever made by
        // a row asking for one. A demoted dictionary counted the stripes before its demotion and
        // none after, so it has no count or frequency of the column to give.
        let (distinct, frequencies, texts) = if dictionary.demoted {
            (None, None, Vec::new())
        } else {
            let distinct = dictionary.counts.iter().filter(|count| **count != 0).count() as u64;
            let (frequencies, texts) = code_frequency(dictionary, &flat, &bases)?;
            (Some(distinct), Some(frequencies), texts)
        };
        // Deriving a fixed SQL host expression at load time materializes its answer.
        let hosts = None;
        drop(flat);
        drop(bases);
        let encoded = encode_global_dictionary(dictionary, &order, &dictionary.placed, true)?;
        let payload = dictionary
            .placed
            .iter()
            .try_fold(0_u64, |sum, place| sum.checked_add(place.length))
            .ok_or_else(|| invalid("global dictionary payload overflow"))?;
        Ok(ClosedDictionary { distinct, frequencies, texts, hosts, encoded, payload })
    }

    /// Writes the statistics sections for the table being closed, as far as the budget reaches.
    ///
    /// Called from [`Self::close`] after the last stripe and after the dictionaries, which is the
    /// first moment the table's column bytes are final and the last moment before the directory is
    /// encoded. Both halves matter: the budget is a share of the column bytes, and a section that
    /// went in after the directory would be a section the directory does not name.
    ///
    /// Nothing here can fail the write. A column whose gather came back blind gets no sections, a
    /// column the budget could not reach gets none, and section 3.1 says both of those plan the way
    /// they planned before statistics existed. The two errors that are returned are an encode
    /// failure and a section count past the bound, and neither is a thing a column can cause.
    fn write_stats(&mut self) -> Result<()> {
        let gathers = std::mem::take(&mut self.gathers);
        let rows = self.table.rows as u64;
        let mut payloads = Vec::new();
        for (column, gather) in gathers.into_iter().enumerate() {
            let Some(gather) = gather else { continue };
            // A gather that saw a different number of rows than the table committed is a gather
            // that missed some, and a distinct count over some of a column is the one error an
            // estimator cannot see coming. This has no way of happening today, since a table is
            // written once and every chunk goes through `flush_pending`, and that is exactly why it
            // is worth a line: it stays true only while that stays true.
            if gather.rows() != rows {
                continue;
            }
            let Some(stats) = gather.finish() else { continue };
            let mut summary = Vec::new();
            stats.summary.encode(&mut summary)?;
            let mut sketches = Vec::new();
            stats.sketches.encode(&mut sketches)?;
            payloads.push((column, summary, sketches));
        }
        if payloads.is_empty() {
            return Ok(());
        }
        let costs = payloads
            .iter()
            .map(|(_, summary, sketches)| summary.len() + sketches.len())
            .collect::<Vec<_>>();
        let allowance = stats::allowance(stats::column_bytes(&self.table), stats::BUDGET_SHARE);
        // Nothing is spent yet. A table this writer is closing is one it wrote from nothing, so the
        // only statistics sections it can have are the ones about to go in.
        let keep = stats::within(&costs, allowance, 0);
        for ((column, summary, sketches), _) in
            payloads.iter().zip(&keep).filter(|&(_, &keep)| keep)
        {
            let id = u64::try_from(*column).map_err(|_| invalid("column index overflow"))?;
            for (kind, bytes, header_bytes) in [
                // A summary is a header the whole way down: there is nothing behind it a reader
                // could decide not to read.
                (*section::SUMMARY, summary, summary.len() as u32),
                (*section::SKETCHES, sketches, rudb_stats::sketches::HEADER_BYTES),
            ] {
                let written = write_section(
                    &*self.file,
                    &mut self.at,
                    &section::Attachment { kind, id, flags: 0, header_bytes, bytes },
                    self.generation,
                )?;
                self.table.sections.push(written);
            }
        }
        if self.table.sections.len() > MAX_SECTIONS {
            return Err(invalid("the table would name more sections than the bound allows"));
        }
        Ok(())
    }

    /// Commits every table this writer has written and syncs the file before publishing its header
    /// slot.
    ///
    /// The table handed back is the one the writer was on, which is the last of them. Callers that
    /// wrote several already know the others, since they named them.
    ///
    /// # Errors
    ///
    /// If directory encoding, writing, or syncing fails.
    pub fn finish(mut self) -> Result<Table> {
        let entry = self.close()?;
        let profile = self.profile.take();
        let _timing = profile.as_deref().map(|profile| profile.span(Stage::Publish));
        let mut tables = std::mem::take(&mut self.closed);
        tables.push(entry);
        let catalog = encode_catalog(&tables, &self.views)?;
        if catalog.len() > MAX_DIRECTORY {
            return Err(invalid("catalog exceeds the configured bound"));
        }
        let offset = self.at;
        self.put(&catalog)?;
        if let Some(profile) = &profile {
            profile.moved(Stage::Publish, 0, catalog.len() as u64, 0);
        }
        // Every page and every table directory is on the disk before anything points at them. The
        // slot write below is what makes this generation the one a reader picks, so the order of
        // these two syncs is the whole of the commit.
        synced(&*self.file, profile.as_deref())?;
        let slot = Slot {
            offset,
            length: u32::try_from(catalog.len()).map_err(|_| invalid("catalog length overflow"))?,
            generation: self.generation,
            hash: checksum(&catalog),
        };
        // The one write that is not an append, and the last one. It goes back over the slot in the
        // header, so it names its offset rather than going through `put`, and `at` does not move.
        // Which of the two slots it is alternates with the generation, so the one naming the
        // generation before this is still intact and still valid until this write lands.
        self.file.write_at(slot_offset(self.generation), &slot.bytes())?;
        synced(&*self.file, profile.as_deref())?;
        Ok(self.table)
    }

    /// Commits a generation that changes the views and leaves every table exactly where it is.
    ///
    /// There was no way to do this before views existed, because everything that could change the
    /// catalog also wrote a table, so the only way to say something new about a file was to go
    /// through a table. A view is the first thing that can change on its own. Without this, adding
    /// a view to a database with eight tables in it would rewrite all eight, since the append path
    /// needs a table to append and the fallback is the whole file.
    ///
    /// It is the same commit as [`Writer::finish`] with nothing appended before it. The table
    /// entries are carried forward by directory pointer the way an append carries them, the new
    /// catalog goes on the end, and the slot write at the end is what publishes it.
    ///
    /// # Errors
    ///
    /// If the file has no valid committed directory, is not this build's format, or cannot be
    /// written.
    pub fn restate(path: impl AsRef<Path>, views: &[ViewEntry]) -> Result<()> {
        let file = RealFilesystem::new().open(path.as_ref(), OpenMode::ReadWrite)?;
        let size = file.len()?;
        let (slot, bytes, _) = committed_slot(&*file, size)?;
        let (closed, _) = decode_catalog(&bytes, size)?;
        let generation = slot
            .generation
            .checked_add(1)
            .ok_or_else(|| invalid("native file generation overflow"))?;
        let catalog = encode_catalog(&closed, views)?;
        if catalog.len() > MAX_DIRECTORY {
            return Err(invalid("catalog exceeds the configured bound"));
        }
        file.write_at(size, &catalog)?;
        file.sync()?;
        let slot = Slot {
            offset: size,
            length: u32::try_from(catalog.len()).map_err(|_| invalid("catalog length overflow"))?,
            generation,
            hash: checksum(&catalog),
        };
        file.write_at(slot_offset(generation), &slot.bytes())?;
        file.sync()?;
        Ok(())
    }

    /// Adds exact count, sum, distinct, bound, and bounded frequency certificates to an older file without
    /// rewriting table pages. The old slot remains readable until the new catalog is synced.
    pub fn certify_summaries(path: impl AsRef<Path>) -> Result<()> {
        let path = path.as_ref();
        let (_, size, slot, bytes, _) = slot_bytes(path)?;
        let (mut entries, views) = decode_catalog(&bytes, size)?;
        let native = Catalog::open(path)?;
        for entry in &mut entries {
            let reader = native.table(&entry.name)?;
            entry.nonzero.fill(None);
            entry.aggregates = reader_aggregate_sums(&reader)?;
            entry.distincts = (0..entry.fields.len())
                .map(|column| reader.distinct_values(column))
                .collect::<Result<Vec<_>>>()?;
            entry.extremes = reader_integer_extremes(&reader)?;
            entry.frequencies = reader_complete_numeric_frequencies(&reader)?;
        }
        let generation = slot
            .generation
            .checked_add(1)
            .ok_or_else(|| invalid("native file generation overflow"))?;
        let catalog = encode_catalog(&entries, &views)?;
        if catalog.len() > MAX_DIRECTORY {
            return Err(invalid("catalog exceeds the configured bound"));
        }
        let file = RealFilesystem::new().open(path, OpenMode::ReadWrite)?;
        file.write_at(size, &catalog)?;
        file.sync()?;
        let slot = Slot {
            offset: size,
            length: u32::try_from(catalog.len()).map_err(|_| invalid("catalog length overflow"))?,
            generation,
            hash: checksum(&catalog),
        };
        file.write_at(slot_offset(generation), &slot.bytes())?;
        file.sync()?;
        Ok(())
    }

    /// The earlier name for [`Self::certify_summaries`].
    pub fn certify_counts(path: impl AsRef<Path>) -> Result<()> {
        Self::certify_summaries(path)
    }
}

/// Appends one run of bytes at `at` and moves it past them, answering where they went.
///
/// The append half of [`attach`], which cannot use [`Writer::put`] because it is not writing a
/// table. Every byte a section costs goes through here, so the offsets in an extent table come
/// from one place.
fn append(file: &dyn rudb_io::File, at: &mut u64, bytes: &[u8]) -> Result<u64> {
    let offset = *at;
    file.write_at(offset, bytes)?;
    *at =
        at.checked_add(bytes.len() as u64).ok_or_else(|| invalid("native file length overflow"))?;
    Ok(offset)
}

/// Writes one attachment's payload as extents and returns the entry that names it.
///
/// The split is by bytes, and the `first` of each extent is therefore a byte count. A section kind
/// whose extents should break on a row boundary instead will want to hand its extents over already
/// split; nothing needs that yet, and guessing at the shape of it now would be guessing.
fn write_section(
    file: &dyn rudb_io::File,
    at: &mut u64,
    one: &section::Attachment<'_>,
    generation: u64,
) -> Result<Section> {
    // A payload of nothing is the exception, and it is not a special case so much as a different
    // reading of the same field: an entry with no bytes has no header to be longer than them, and
    // `header_bytes` is what the structure would have cost. See `Section::refused`.
    if !one.bytes.is_empty() && one.header_bytes as usize > one.bytes.len() {
        return Err(invalid("a section's header is longer than its payload"));
    }
    let mut extents = Vec::new();
    let mut first = 0_u64;
    let extent_size =
        if one.kind == *section::SORTED_PROJECTION || one.kind == *section::RUN_PROJECTION {
            1 << 19
        } else {
            section::MAX_EXTENT as usize
        };
    for chunk in one.bytes.chunks(extent_size) {
        let offset = append(file, at, chunk)?;
        extents.push(section::Extent {
            offset,
            length: u32::try_from(chunk.len()).map_err(|_| invalid("extent length overflow"))?,
            hash: checksum(chunk),
            first,
        });
        first += chunk.len() as u64;
    }
    let mut table = Vec::with_capacity(extents.len() * section::EXTENT_BYTES);
    section::encode_extents(&extents, &mut table)?;
    // A payload of nothing is a section of no extents and no extent table, and its `extent_page`
    // is zero rather than the end of the file. Section 3.7 wants that entry to exist: it is how a
    // relationship that did not fit the budget is recorded as not built rather than forgotten.
    let extent_page = if table.is_empty() { 0 } else { append(file, at, &table)? };
    Ok(Section {
        kind: one.kind,
        id: one.id,
        generation,
        extents: u32::try_from(extents.len()).map_err(|_| invalid("too many extents"))?,
        extent_page,
        extent_bytes: u32::try_from(table.len()).map_err(|_| invalid("extent table overflow"))?,
        hash: checksum(&table),
        flags: one.flags,
        header_bytes: one.header_bytes,
    })
}

/// Attaches graph sections to a table already committed in a file, without rewriting a page.
///
/// This is the second pass spec/graph/03-the-file-format.md section 3.8 asks for. A key map has to
/// exist before the link that uses it can be built, and it is built by reading the key column back,
/// so the structures of a table cannot be written during the load that wrote the table. They are
/// written afterwards, by this, and the file in between the two is a correct file that answers
/// every query more slowly.
///
/// Nothing is overwritten. The payloads, the extent tables, the new directory for this table and
/// the new catalog all go on the end of the file past the committed generation, and the last write
/// is the header slot, exactly as [`Writer::finish`] does it. So a crash anywhere in here leaves
/// the generation before it intact, and leaves unreferenced trailing bytes that the next commit
/// writes past.
///
/// An attachment replaces any section of the same kind and id, and every other section is carried
/// through untouched, including one whose kind this build does not know. The table's own generation
/// is carried through too, because attaching a section moves no row: see [`Table::generation`].
///
/// # Errors
///
/// If the file has no valid committed directory, is an older format than this build writes, holds
/// no table of that name, names a section whose payload cannot be written, or would end up naming
/// more sections than the format allows.
pub fn attach(
    path: impl AsRef<Path>,
    table: &str,
    attachments: &[section::Attachment<'_>],
) -> Result<Table> {
    let file = RealFilesystem::new().open(path.as_ref(), OpenMode::ReadWrite)?;
    let file = &*file;
    let size = file.len()?;
    let (slot, bytes, _) = committed_slot(file, size)?;
    let (mut entries, views) = decode_catalog(&bytes, size)?;
    let at = entries
        .iter()
        .position(|entry| entry.name == table)
        .ok_or_else(|| invalid(&format!("the file holds no table called {table}")))?;
    let mut version = [0; 4];
    read_at(file, 8, &mut version)?;
    let version = u32::from_le_bytes(version);
    // Readable is not the same as writable. A format 22 file has no section table, and giving its
    // directory one without moving the number in its header would leave a file that claims to be
    // format 22 and is not, which is worse than refusing. Rewriting it with this build is the
    // answer, and the format is at 0.3.x, so nobody has one of these that this project did not
    // just make.
    if version != FORMAT {
        return Err(invalid(&format!(
            "the file is format {version} and a graph section needs format {FORMAT}, so it has \
             to be written again"
        )));
    }
    let mut directory = vec![0; entries[at].directory.length as usize];
    read_at(file, entries[at].directory.offset, &mut directory)?;
    if checksum(&directory) != entries[at].directory.hash {
        return Err(invalid(&format!("the directory of table {table} does not checksum")));
    }
    let mut held = decode_directory(&directory, size)?;
    let mut cursor = size;
    for one in attachments {
        let written = write_section(file, &mut cursor, one, held.generation)?;
        held.sections.retain(|old| !(old.kind == one.kind && old.id == one.id));
        held.sections.push(written);
    }
    if held.sections.len() > MAX_SECTIONS {
        return Err(invalid("the table would name more sections than the bound allows"));
    }
    let encoded = encode_directory(&held)?;
    if encoded.len() > MAX_DIRECTORY {
        return Err(invalid("directory exceeds the configured bound"));
    }
    let offset = append(file, &mut cursor, &encoded)?;
    entries[at].directory = Page {
        offset,
        length: u32::try_from(encoded.len()).map_err(|_| invalid("directory length overflow"))?,
        hash: checksum(&encoded),
    };
    // The views the file already had, written back unchanged. Attaching a section to a table says
    // nothing about a view and must not drop one.
    let catalog = encode_catalog(&entries, &views)?;
    if catalog.len() > MAX_DIRECTORY {
        return Err(invalid("catalog exceeds the configured bound"));
    }
    let offset = append(file, &mut cursor, &catalog)?;
    file.sync()?;
    let generation =
        slot.generation.checked_add(1).ok_or_else(|| invalid("native file generation overflow"))?;
    let committed = Slot {
        offset,
        length: u32::try_from(catalog.len()).map_err(|_| invalid("catalog length overflow"))?,
        generation,
        hash: checksum(&catalog),
    };
    file.write_at(slot_offset(generation), &committed.bytes())?;
    file.sync()?;
    Ok(held)
}

/// One column's frequency synopsis as values with their row counts, shared by every clone of a
/// reader.
type Synopsis = Arc<Vec<(Value, u64)>>;

/// Reads committed native column pages without holding the table in memory.
#[derive(Debug, Clone)]
pub struct Reader {
    file: Arc<File>,
    table: Arc<Table>,
    dictionaries: Arc<Vec<OnceLock<Arc<Vector>>>>,
    /// Held while a global dictionary is being opened, one per column.
    ///
    /// The [`OnceLock`] above says whether one has been opened, which is the question a reader that
    /// already has it needs answered and is free. It does not say whether one is being opened, and
    /// the difference matters because every worker of a scan wants the same dictionary at the same
    /// moment. Without this they all miss, all read the page, all verify it and all decode it, and
    /// all but one throw the answer away. ClickBench 38 reads the URL dictionary, which is 515,958
    /// entries, and was paying for it twice.
    loading: Arc<Vec<Mutex<()>>>,
    /// Each column's frequency synopsis as values, the first time anything asks for it. See
    /// [`Reader::decode_frequencies`].
    frequency_values: Arc<Vec<OnceLock<Synopsis>>>,
    /// Stored frequency sections are decoded once per open table. A small directory can hold the
    /// summary inline, but a larger one otherwise rereads and decodes the same section on every
    /// plan and every summary-backed aggregate.
    frequency_summaries: Arc<Vec<OnceLock<Arc<FrequencySummary>>>>,
    /// How many global dictionaries have been opened. A scan of a dictionary column should open its
    /// dictionary once however many workers it has, and the test that says so is the only thing
    /// keeping it that way.
    opened: Arc<AtomicUsize>,
    /// The membership sieves of one stripe of one column, by column and then by stripe, read the
    /// first time a probe asks about them. A query filters on one or two columns and never looks at
    /// the rest, so reading these at open would be the whole index for the sake of a fraction of it.
    sieves: Arc<Vec<Vec<SieveSlot>>>,
    /// The per part ranges of one stripe of one column, by column and then by stripe, read the
    /// first time something compares that column and kept after that.
    part_ranges: Arc<Vec<Vec<RangeSlot>>>,
    /// Which stripe and which part of it every part of the table is, by table wide part number.
    places: Arc<Vec<Place>>,
    cache: Arc<Shelf>,
    /// Where the pages above are counted against the database's budget. See [`PagePool`].
    pool: PagePool,
    /// How many whole stripe pages have been read, which is what the sharing above is judged on. A
    /// scan of a column should read each of its stripes once however many workers it has.
    pages: Arc<AtomicUsize>,
    /// How many index sections have been read. A scan of a column should read each of its stripes
    /// once here too, and the test that says so is the only thing keeping it that way.
    indexes: Arc<AtomicUsize>,
    /// The file's size when it was opened, for [`Reader::layout`].
    size: u64,
    /// The committed directory's size, for [`Reader::layout`].
    directory: u64,
    /// What opening the file cost, which is a number rather than a claim.
    opening: Opening,
}

/// What [`Reader::open`] read before it returned.
///
/// `spec/stats/04-in-memory.md` section 4.2 says opening a table reads the header and the directory
/// and nothing else, and once that document's statistics are in the file the tempting change is to
/// load a column summary or two on the way past, because they are small and the next query will
/// want them. A hundred milliseconds of that is a hundred milliseconds nobody asked for, and an
/// embedded database is opened by processes that are about to run one trivial query.
///
/// So the claim gets a number. Both of these are fixed by the schema and the stripe count and are
/// independent of how many rows the file holds, and the test that says so is what stops the
/// tempting change from landing quietly.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Opening {
    /// How many times the file was read. The header, then each directory slot that looked valid
    /// enough to check, so three at the most.
    pub reads: u32,
    /// How many bytes those reads asked for.
    pub bytes: u64,
}

/// What a reader has read, while it was being opened and since.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Reads {
    /// What opening cost, before any query had been planned.
    pub opening: Opening,
    /// Whole stripe pages read since.
    pub pages: usize,
    /// Index sections read since.
    pub indexes: usize,
    /// Global dictionaries opened since. One per dictionary column that a query touched, however
    /// many workers touched it, which is a claim only a test can keep true.
    pub dictionaries: usize,
}

/// Where one table wide part number lands.
#[derive(Debug, Clone, Copy)]
struct Place {
    stripe: u32,
    part: u32,
    rows: u32,
}

/// One part's bytes inside one column page.
#[derive(Debug, Clone, Copy)]
struct PartSpan {
    start: usize,
    length: usize,
    hash: u64,
}

/// What a reader holds for one stripe of one column.
///
/// The index is small and is loaded whether the caller wants the whole page or one part of it. The
/// page is loaded only by a scan, because a sparse fetch that wants a thousand rows out of sixty
/// four thousand would be reading sixty four times what it uses.
#[derive(Debug, Clone)]
struct CachedColumn {
    stripe: usize,
    index: Arc<Vec<PartSpan>>,
    page: Option<Arc<HeldPage>>,
}

/// One stripe's page of one column, with which of its parts have already matched their checksums.
///
/// The bytes never change once they are read, so a part that matched once matches for as long as
/// the page is held. Hashing it again on every read was 3.5% of a `GROUP BY CounterID` over the
/// held pages of the ClickBench sample, run seventy times in one process. A part read without its
/// page is still checked every time, since those bytes come fresh off the file.
#[derive(Debug)]
struct HeldPage {
    bytes: Vec<u8>,
    checked: Vec<AtomicBool>,
}

impl HeldPage {
    /// The bytes of part `part`, checked against `span` the first time anyone asks for them.
    fn part(&self, part: usize, span: PartSpan) -> Result<&[u8]> {
        let bytes = part_bytes(&self.bytes, span)?;
        let checked = self.checked.get(part).ok_or_else(|| invalid("part index out of range"))?;
        if !checked.load(Atomic::Relaxed) {
            verify_part(bytes, span)?;
            checked.store(true, Atomic::Relaxed);
        }
        Ok(bytes)
    }
}

/// Checks one part's bytes against the hash its index carries for them.
fn verify_part(bytes: &[u8], span: PartSpan) -> Result<()> {
    let got = checksum(bytes);
    if got != span.hash {
        return Err(invalid(&format!(
            "column page checksum differs, part at {}+{} bytes, wanted {:016x} and got {got:016x}",
            span.start, span.length, span.hash,
        )));
    }
    Ok(())
}

/// One column's stripes a reader holds, and which of them somebody is reading right now.
///
/// The pages are one slot per stripe of the table rather than a list of the ones being kept, so
/// finding a page is an index and not a walk. That matters because the walk happened under the
/// lock, once per part per column, and a scan that gives a whole stripe to each of thirty two
/// workers keeps enough pages that walking them was the longest thing the lock was held for. The
/// slots cost a pointer per stripe per column, which on the ClickBench file is eight kilobytes
/// against the forty megabytes of pages they point at. `order` is which of them are filled, oldest
/// first, because that is the one thing the slots cannot say by themselves.
///
/// `loading` is what keeps a scan from reading the same page once per worker. It is a list and not
/// a set because it holds at most one stripe per worker on the column and is walked far less often
/// than a hash of it would be built.
///
/// `index` is every index this reader has ever read for the column, one slot per stripe, and it is
/// never evicted. An index is a few hundred bytes and a page is a quarter of a megabyte, so the two
/// do not belong under the same budget. Riding in the page cache meant a worker that came back to a
/// stripe after its page had been evicted read the index again with it, which on the full
/// ClickBench file was about thirteen hundred reads out of a hundred and fourteen thousand.
///
/// `seen` is which stripes have had their page read before, and `passing` is the pages read for the
/// first time that are still held, oldest first. A page goes into the pool the second time it is
/// read and not the first, which is the rule [`NativeText`] follows for its decoded blocks. A
/// process that runs one statement, which is how a script or a benchmark uses the engine, reads
/// each page once, and with no memory limit the pool kept every one of them to the end: ClickBench
/// q33 held all of `WatchID` and `ClientIP` at its peak for a second scan that never came. The
/// first read now keeps a page only while it is among the column's floor of newest ones, and a
/// session that scans the table again pays one more read of each page and keeps it from then on.
#[derive(Debug, Default)]
struct Cached {
    pages: Vec<Option<Resident>>,
    loading: Vec<usize>,
    index: Vec<Option<Arc<Vec<PartSpan>>>>,
    seen: Vec<bool>,
    passing: VecDeque<usize>,
}

/// One page a reader holds, and whether anyone has read it since the pool last looked.
#[derive(Debug, Clone)]
struct Resident {
    page: Arc<HeldPage>,
    used: Arc<AtomicBool>,
}

/// Every column's pages of one reader, with how many each column holds and the floor under that.
#[derive(Debug)]
struct Shelf {
    columns: Vec<Mutex<Cached>>,
    /// How many pages each column holds right now. Counted outside the column locks so that the
    /// pool can tell whether a column is at its floor without taking a lock it might be under.
    held: Vec<AtomicUsize>,
    /// How many stripes of one column are kept whatever the budget says. See
    /// [`CACHED_STRIPES_PER_COLUMN`] for what sets it and [`Reader::keep_stripes`] for who raises it.
    kept: AtomicUsize,
}

/// The pages every reader of one database keeps, under one budget in bytes.
///
/// A reader lives as long as the database does, so the pages it holds are what the next query finds
/// already in memory. They used to be four stripes a column, oldest out first, which on TPC-H SF1
/// meant every query read every page of lineitem off the file again and paid the system call for
/// it. Keeping every page there costs 38 MB and took a third of the system time off the suite.
///
/// So the question is no longer how many stripes a column keeps but how many bytes the database
/// does, and one budget answers it for every reader at once. A table nobody queries gives its pages
/// up to one that is being queried, which a count per column cannot do.
///
/// Pages leave by the clock. Each has a bit a read sets, and when the pool is over budget it walks
/// from the oldest: a page with the bit set loses the bit and goes round again, and a page without
/// it goes. That keeps what is read over and over and lets a page one scan read once go first.
///
/// The old count is still a floor. A column never gives up a page while it holds four or fewer,
/// because a scan whose workers evict each other's pages reads a quarter of a megabyte for every
/// part it takes, and a budget of zero is the cache as it was before the pool existed.
#[derive(Debug, Clone, Default)]
pub struct PagePool {
    ring: Arc<Mutex<Ring>>,
    budget: Arc<AtomicUsize>,
}

#[derive(Debug, Default)]
struct Ring {
    held: VecDeque<Held>,
    bytes: usize,
}

/// One page in the pool, pointing back at the reader that holds it.
///
/// Weak, because a reader that has gone, which every reader does at a checkpoint, should take its
/// pages with it and not have them kept alive by the pool.
#[derive(Debug)]
struct Held {
    shelf: Weak<Shelf>,
    column: usize,
    stripe: usize,
    bytes: usize,
    used: Arc<AtomicBool>,
}

impl PagePool {
    /// A pool that keeps up to `budget` bytes of pages beyond each column's floor.
    #[must_use]
    pub fn new(budget: usize) -> Self {
        let pool = Self::default();
        pool.budget.store(budget, Atomic::Relaxed);
        pool
    }

    /// The bytes of pages the pool is counting now.
    ///
    /// # Panics
    ///
    /// If the pool's lock is poisoned, which takes a panic while it was held.
    #[must_use]
    pub fn bytes(&self) -> usize {
        self.ring.lock().map_or(0, |ring| ring.bytes)
    }

    /// Counts a page a reader has just taken in, and lets pages go until the pool is back under its
    /// budget or it has looked at every page once.
    ///
    /// Called with no column lock held. The pages that go are chosen under the pool's lock and
    /// dropped under their column's lock afterwards, so no thread ever holds both.
    fn admit(&self, held: Held) {
        let budget = self.budget.load(Atomic::Relaxed);
        let mut gone = Vec::new();
        {
            let Ok(mut ring) = self.ring.lock() else { return };
            ring.bytes += held.bytes;
            ring.held.push_back(held);
            // One lap and no more. A page read since the last pass loses its bit on this one and
            // can only go on a later one, which is the second chance the clock is named for.
            let mut looked = 0;
            let limit = ring.held.len();
            while ring.bytes > budget && looked < limit {
                looked += 1;
                let Some(entry) = ring.held.pop_front() else { break };
                let Some(shelf) = entry.shelf.upgrade() else {
                    ring.bytes -= entry.bytes;
                    continue;
                };
                if entry.used.swap(false, Atomic::Relaxed) {
                    ring.held.push_back(entry);
                    continue;
                }
                let count = &shelf.held[entry.column];
                if count.load(Atomic::Relaxed) <= shelf.kept.load(Atomic::Relaxed).max(1) {
                    ring.held.push_back(entry);
                    continue;
                }
                count.fetch_sub(1, Atomic::Relaxed);
                ring.bytes -= entry.bytes;
                gone.push((shelf, entry));
            }
            // A reader that has gone leaves its entries behind, and with a budget nobody reaches
            // they would pile up one checkpoint after another. The front is where the oldest are.
            while ring.held.front().is_some_and(|entry| entry.shelf.strong_count() == 0) {
                if let Some(entry) = ring.held.pop_front() {
                    ring.bytes -= entry.bytes;
                }
            }
        }
        for (shelf, entry) in gone {
            let Ok(mut cached) = shelf.columns[entry.column].lock() else { continue };
            if let Some(slot) = cached.pages.get_mut(entry.stripe) {
                if slot.as_ref().is_some_and(|slot| Arc::ptr_eq(&slot.used, &entry.used)) {
                    *slot = None;
                }
            }
        }
    }
}

/// Stripes of one column a reader keeps the bytes of, when nobody has asked for more.
///
/// This has to hold at least as many stripes as a column has workers in it at once, or the workers
/// evict each other's pages and read them again. Four is what a scan that hands parts out in order
/// needs, because then every worker is within a few parts of every other and at most a couple of
/// stripes are open at a time. A scan that hands a whole stripe to each worker has one stripe open
/// per worker for the length of that stripe, and it says so with [`Reader::keep_stripes`] rather
/// than paying for sixteen slots on every table that is read one part at a time.
///
/// It multiplies by the page size, which is a quarter of a megabyte for a four byte column, and by
/// the number of columns a query touches.
const CACHED_STRIPES_PER_COLUMN: usize = 4;

/// The sieves of one stripe of one column, once somebody has asked for them.
type SieveSlot = OnceLock<Arc<Vec<Option<Sieve>>>>;

type RangeSlot = OnceLock<Arc<Vec<Range>>>;

#[derive(Debug)]
struct NativeText {
    file: Arc<File>,
    /// How many values the dictionary holds.
    values: usize,
    /// Where each value ends inside its payload block, packed at `offset_bits` in runs of
    /// [`TEXT_OFFSET_RUN`].
    ///
    /// Ends rather than starts, because then a block of 1,024 values is 1,024 numbers rather than
    /// 1,025: the start of a value is the end of the one before it, and the first value of a block
    /// starts at zero by construction. Relative to the block rather than to the payload, because a
    /// reader decodes a whole block and slices it, so an offset into the payload is a number it
    /// would have to subtract a base from anyway.
    ///
    /// The vector is the index as it was read, so the offsets start after the header, and
    /// [`Self::packed`] is where they are read from.
    offsets: Vec<u8>,
    /// Bits one offset is packed at, which is what the largest block of this column spans and is the
    /// same for every block of it.
    offset_bits: usize,
    /// The same ends unpacked, built once enough readers have asked for one at a time.
    ///
    /// Reading one offset out of the packed form costs about fifty instructions: a division to find
    /// the run, a bounds check to slice it, a shift to reach the bit the value starts at and a
    /// narrowing on the way out. That is the right price for a reader that wants a handful. It is
    /// the wrong price for `STRLEN` over a column, which asks for one per row and nothing else, and
    /// where a million of them was a third of ClickBench 28.
    ///
    /// With the ends unpacked every read is a load, and a vector of lengths is one loop over them.
    /// The table is built only once the reads say it will be used, which is what
    /// [`Self::ends_worth_unpacking`] decides and [`Self::ends_asked`] counts towards, because a
    /// table built for a reader that wanted three values is four bytes a value spent on nothing.
    value_ends: OnceLock<Option<Vec<u32>>>,
    /// The length of every value, worked out of [`Self::value_ends`] the first time a vector of
    /// lengths is asked for.
    ///
    /// A length out of the ends is two loads, a test for whether the value opens its block and a
    /// check that it does not end before it starts, which came to thirteen instructions a row on
    /// ClickBench 28. Out of this it is one load. The order is checked once for the whole table
    /// while it is built, and a column that fails it gets no table and goes on reading the ends,
    /// which is where the error is reported. Two bytes a value where every value is short enough,
    /// four otherwise, and only for a column something has asked the length of a vector at a time.
    value_lens: OnceLock<Option<Lengths>>,
    /// How many single offset reads have come in while the table is not built.
    ///
    /// Relaxed, and read only against a threshold, so two threads racing here means the table is
    /// built one read early or one read late. Counting stops the moment the table exists, because
    /// [`OnceLock::get`] settles it before this is touched.
    ends_asked: AtomicUsize,
    /// How many entries the sorted order has, which is the value count.
    ranks: usize,
    /// Where the sorted order starts in the file. It is read a block at a time and only when
    /// something searches it, so a query that never compares this column against a literal never
    /// touches it at all.
    rank_at: u64,
    /// Where each block of the sorted order ends, as a byte offset from `rank_at`. A block is packed
    /// at whatever width its own heads need, so unlike the entries it replaced its length is not
    /// arithmetic on the block number.
    rank_ends: Vec<u64>,
    rank_hashes: Vec<u64>,
    rank_blocks: Vec<OnceLock<Result<Vec<u8>>>>,
    /// Bits one code is packed at, which is what the value count needs and is the same for every
    /// block of the column.
    code_bits: usize,
    /// The sorted order turned round, built the first time a reader asks for it.
    ///
    /// Four bytes per value against the four the offsets already hold, so a column that has this is
    /// carrying half again what it carried before rather than something of a new order. It is built
    /// only when something asks, which is a grouped min or max over this column and nothing else,
    /// and that reader was going to read the payload of this column once per row otherwise.
    code_ranks: OnceLock<Option<Vec<u32>>>,
    /// Where each block of the payload starts in the file, and how many stored bytes it is.
    ///
    /// Absolute rather than an offset from a base the blocks share, because a block is written the
    /// moment it fills and what comes after it in the file is whatever the load wrote next. A file
    /// old enough to have them back to back is read into these same two lists by adding the base to
    /// the ends it carries, so nothing below here knows which kind of file it came from.
    starts: Vec<u64>,
    lengths: Vec<u64>,
    hashes: Vec<u64>,
    /// Conservative four-byte substring signatures, read only by a compatible LIKE filter.
    grams: Option<NativeGrams>,
    /// The payload, read and decoded a block at a time and kept after that.
    blocks: Vec<OnceLock<Result<Vec<u8>>>>,
    /// The length in characters of every value of a block, worked out the first time `length` asks
    /// for a value in that block.
    ///
    /// Kept instead of the block it was counted out of. `length` reads every row of a column, and
    /// reading the bytes through [`Self::payload_block`] kept every block it touched, which is every
    /// distinct value of the column decoded: seven string columns of ClickBench held 13.9 GB to
    /// answer seven `max(length(...))`. The counts are four bytes a value, so the same scan keeps
    /// the counts and decodes each block once, the same number of times it did before.
    char_lens: Vec<OnceLock<Box<[u32]>>>,
    /// How many decoded payload bytes this column keeps before a sweep stops keeping what it reads.
    /// [`TEXT_KEEP_BUDGET`] everywhere but in the test of the ceiling.
    keep_budget: usize,
    /// Roughly how many decoded payload bytes are being kept, which is what [`TEXT_KEEP_BUDGET`]
    /// is measured against.
    ///
    /// Roughly, because two threads that keep the same block at the same time both add its length
    /// while [`OnceLock`] keeps one of the two. That makes the count read high and the budget bind
    /// a little early, which is the harmless direction, and it costs one relaxed add a block rather
    /// than a lock on the path every scan of a string column goes through.
    payload_kept: AtomicUsize,
    /// Which payload blocks a sweep has decoded before, one flag a block.
    ///
    /// A sweep keeps a block the second time it decodes it and not the first. A process that runs
    /// one statement, which is how a benchmark or a script uses the engine, sweeps each block once
    /// and so keeps nothing: on ten million rows a `URL LIKE` held 396 MB with every block kept and
    /// 97 MB with none, for the same processor time. A session that asks again pays the decode one
    /// more time and reads kept blocks from then on, under the same [`TEXT_KEEP_BUDGET`].
    swept: Vec<AtomicBool>,
    /// How many blocks [`TextSource::visit_at`] has decoded and dropped because the column was
    /// already holding its [`TEXT_KEEP_BUDGET`].
    ///
    /// A sweep reads the dictionary in order and touches a block once, so dropping what it reads
    /// past the budget costs one decode a block and bounds the column. A visit reads a vector of
    /// codes, and the codes of a scan land all over the dictionary: on ten million rows of
    /// ClickBench each vector of two thousand `URL`s touches about a hundred and forty of its two
    /// and a half thousand blocks, and so does the next one. A cache holding a tenth of the column
    /// still misses half of those, and dropping every block past the budget would decode the
    /// column hundreds of times over to answer one `lower(URL)`. So a visit drops past the budget
    /// only until it has dropped as many blocks as the column has, which is what a read whose codes
    /// are few or clustered never reaches, and keeps what it reads after that, the way a row at a
    /// time read always did. That bounds what a visit can cost over the old read at one more decode
    /// of the column.
    visit_dropped: AtomicUsize,
    /// The boundaries this dictionary has already been searched for, by the value searched for.
    ///
    /// A search is the expensive thing this type does. It settles a probe on the stored head where
    /// it can and reads a value where it cannot, and reading a value decodes the payload block it
    /// sits in, so one search can cost several blocks. The thing that makes remembering worth it is
    /// that the same search comes back: a top N asks once a chunk whether anything left can beat its
    /// worst candidate, and the worst candidate settles long before the chunks run out.
    ///
    /// Shared across the instances of a scan rather than kept per instance, because each of them has
    /// its own worst candidate and all of them are searching the same dictionary. One lock per chunk
    /// is nothing next to a probe of a file.
    ///
    /// Bounded by [`TEXT_SEARCH_MEMO`] and emptied rather than evicted when it is full. What fills
    /// it is a top N improving its bound, which happens a few dozen times and then stops, so the
    /// bound is there for the filter that searches for a different literal every chunk rather than
    /// for anything this is meant to help.
    searched: Mutex<HashMap<Vec<u8>, (usize, bool)>>,
}

#[derive(Debug)]
struct NativeGrams {
    start: u64,
    length: usize,
    /// How long one block's signature is.
    width: usize,
    hash: u64,
    /// For each literal asked about lately, whether each block might hold it.
    ///
    /// The answer for every block at once, worked out by one pass over the signatures a window at a
    /// time, rather than the signatures read in and kept. On ClickBench `URL` they are 21 MB for
    /// ten million rows and a verdict is 2,650 flags, and a filter asks the same question of every
    /// block, so the pass is paid once and what stays resident is the flags.
    verdicts: Mutex<Vec<Verdict>>,
}

/// A literal and whether each block might hold it.
type Verdict = (Vec<u8>, Arc<[bool]>);

/// How many literals a column remembers the verdicts of.
const GRAM_VERDICTS: usize = 8;

impl NativeGrams {
    /// Whether each block might hold `literal`, remembered or worked out now.
    ///
    /// The lock is held over the pass so that the threads of one scan, which all ask about the
    /// same literal at the start, read the signatures once between them.
    fn verdicts(&self, file: &File, literal: &[u8]) -> Result<Arc<[bool]>> {
        let mut held = self.verdicts.lock().map_err(|_| invalid("a poisoned signature verdict"))?;
        if let Some((_, verdict)) = held.iter().find(|(asked, _)| asked == literal) {
            return Ok(Arc::clone(verdict));
        }
        let wanted = literal.windows(4).map(|gram| gram_bits(gram, self.width)).collect::<Vec<_>>();
        let mut verdict = Vec::with_capacity(self.length / self.width);
        let window = GRAM_WINDOW / self.width * self.width;
        let hash = walk_checksummed(file, self.start, self.length, window, |bytes| {
            verdict.extend(bytes.chunks(self.width).map(|bits| {
                wanted
                    .iter()
                    .flatten()
                    .all(|&bit| bits.get(bit / 8).is_some_and(|byte| byte & (1 << (bit % 8)) != 0))
            }));
            Ok(())
        })?;
        if hash != self.hash {
            return Err(invalid("global dictionary substring signatures checksum differs"));
        }
        let verdict: Arc<[bool]> = verdict.into();
        if held.len() >= GRAM_VERDICTS {
            held.remove(0);
        }
        held.push((literal.to_vec(), Arc::clone(&verdict)));
        Ok(verdict)
    }

    fn footprint(&self) -> usize {
        self.verdicts.lock().map_or(0, |held| {
            held.iter().map(|(asked, verdict)| asked.capacity() + verdict.len()).sum()
        })
    }
}

/// How many searched for values a column's dictionary remembers the boundary of.
///
/// See [`NativeText::searched`]. Small because the case it is for repeats one value, not because a
/// larger one would be wrong.
const TEXT_SEARCH_MEMO: usize = 64;

/// How many values of a dictionary go in one block of the payload.
///
/// The block is the unit the string cascade encodes, the unit a checksum covers, and the unit a
/// reader has to decode to get at a single value, so it is the one number the payload format turns
/// on. Blocking by values rather than by bytes is what keeps a value out of two blocks at once: the
/// block holding a code is `code / TEXT_PAYLOAD_VALUES` and nothing has to be stitched.
///
/// A probe on the five ClickBench columns that have a dictionary worth the name, written up on
/// #347, measured the ratio and the decode speed at 128, 256, 512, 1,024 and 4,096 values. Both get
/// better all the way up, because front coding and the LZ matcher have more to look back at and
/// because the per chunk setup is spread over more values. What stops it is the point read: a query
/// that wants ten values has to decode ten blocks, so the block is what a lookup costs. At 1,024
/// values a block is between 67 KB and 394 KB decoded across those five columns, and the ratios are
/// 2.3 to 4.5. Going up to 4,096 buys two to six percent more and makes a block as much as 1.5 MB.
/// Going down to 512 gives up five to nine percent.
const TEXT_PAYLOAD_VALUES: usize = 1024;

/// Eight KiB per payload block, which is what makes a four-byte substring a useful negative test on
/// a column of URLs.
///
/// Two KiB was the first answer and on ClickBench `URL` it proved almost nothing. A block of 1,024
/// sorted URLs holds about seventeen thousand distinct four-byte grams, and at two bits each that
/// set nine in ten of the sixteen thousand bits there were, so `LIKE '%google%'` passed most blocks
/// it had no match in and decoded them. At eight KiB four bits in ten are set, and of the 2,650
/// blocks of `URL` in ten million rows a needle that is in none of them passes 36. The signatures
/// are not read into memory, see [`NativeGrams::verdicts`], so the width costs file and not
/// resident memory.
const TEXT_GRAM_BYTES: usize = 8192;

/// The signature width of a format 28 file, which is still read.
const NARROW_GRAM_BYTES: usize = 2048;

/// How much of a column's signatures a verdict reads at a time.
const GRAM_WINDOW: usize = 256 << 10;

/// A fast mixing step for exactly four bytes, shared by load and query, into a signature of
/// `width` bytes.
fn gram_bits(bytes: &[u8], width: usize) -> [usize; 2] {
    let original = u32::from_le_bytes(bytes.try_into().expect("a four-byte gram"));
    let mut first = original ^ (original >> 16);
    first = first.wrapping_mul(0x7feb_352d);
    first ^= first >> 15;
    let mut second = original ^ (original >> 17);
    second = second.wrapping_mul(0x846c_a68b);
    second ^= second >> 16;
    let mask = width * 8 - 1;
    [(first as usize) & mask, (second as usize) & mask]
}

/// How many decoded payload bytes one dictionary keeps before a sweep stops keeping what it reads.
///
/// A sweep of the whole dictionary decodes every block whatever it does, and the only question is
/// whether it hangs on to them. Keeping all of them is 4.2 GB on ClickBench `URL` at a hundred
/// million rows, which is what #997 was right to stop. Keeping none of them means the next query
/// asking the same thing decodes all of it again, and on the same column at a million rows that
/// took a `LIKE` from 2.7 ms to 16.2 ms, because the decode used to be paid once by a session and
/// is now paid by every statement in it. Neither end is the answer. A bound is.
///
/// So a sweep keeps what it decodes until the column is holding this much and decodes without
/// keeping after that. At a million rows the five ClickBench string columns decode to between 8 MB
/// and 85 MB, so they sit inside it and a repeated `LIKE` reads a decoded block rather than a
/// stored one. At a hundred million rows `URL` fills it and the rest of that column is read and
/// dropped, which is the old cost on the part that does not fit and none of the old footprint.
///
/// Two hundred and fifty six megabytes a column is a number and not a policy, and the policy is
/// what should replace it: this wants to be a buffer pool over the whole database, sized against
/// the memory limit the session was given, with the blocks of every column competing for it and the
/// least useful one evicted. That is F2 work. What is here is the part of it that can be written
/// without an eviction order, which is a ceiling.
const TEXT_KEEP_BUDGET: usize = 256 * 1024 * 1024;

/// The length of every value of a column, as narrow as the longest of them allows.
///
/// The table is read at the codes a vector holds, which on a column the size of ClickBench `URL`
/// land all over it, so what a length costs is whether its line is in cache. Half a million URLs
/// are two megabytes at four bytes a length and one at two, which is the difference between the
/// table sitting in the second level cache or not.
#[derive(Debug)]
enum Lengths {
    /// Every length fits in sixteen bits.
    Narrow(Vec<u16>),
    /// Some value is longer than that.
    Wide(Vec<u32>),
}

impl Lengths {
    /// The lengths at `indices`, appended to `into`, and zero for a position past the end, which
    /// is what a row at a time read says.
    fn extend_at(&self, indices: &[u32], into: &mut Vec<i64>) {
        match self {
            Lengths::Narrow(lens) => into.extend(
                indices
                    .iter()
                    .map(|&index| lens.get(index as usize).map_or(0, |&len| i64::from(len))),
            ),
            Lengths::Wide(lens) => into.extend(
                indices
                    .iter()
                    .map(|&index| lens.get(index as usize).map_or(0, |&len| i64::from(len))),
            ),
        }
    }

    /// The bytes the table holds on to.
    fn footprint(&self) -> usize {
        match self {
            Lengths::Narrow(lens) => lens.capacity() * size_of::<u16>(),
            Lengths::Wide(lens) => lens.capacity() * size_of::<u32>(),
        }
    }
}

/// The length of every value out of where each one ends inside its payload block, or `None` for
/// ends that go backwards somewhere inside a block.
///
/// A value that opens a block starts at zero and every other one starts where the value before it
/// ends, so a block is a run of differences.
///
/// Built at two bytes a length straight away, and built again at four only when some value turns
/// out too long for that, which is rare enough that the second pass is not worth avoiding.
fn lengths_of(ends: &[u32]) -> Option<Lengths> {
    match lengths_as::<u16>(ends)? {
        Some(narrow) => Some(Lengths::Narrow(narrow)),
        None => lengths_as::<u32>(ends)?.map(Lengths::Wide),
    }
}

/// [`lengths_of`] at one width: `None` for ends that go backwards, and `Some(None)` for a length
/// that does not fit in `T`.
fn lengths_as<T: TryFrom<u32>>(ends: &[u32]) -> Option<Option<Vec<T>>> {
    let mut lens = Vec::with_capacity(ends.len());
    for block in ends.chunks(TEXT_PAYLOAD_VALUES) {
        let mut start = 0;
        for &end in block {
            let Ok(len) = T::try_from(end.checked_sub(start)?) else {
                return Some(None);
            };
            lens.push(len);
            start = end;
        }
    }
    Some(Some(lens))
}

/// How many offsets go in one packed run.
///
/// A payload block holds 1,024 values and `bitpack::pack_tail` takes fewer than 1,024 at a time,
/// since a whole unit of that many belongs in the transposed layout instead. So the offsets of a
/// block go in two runs. Five hundred and twelve values at any width is a whole number of bytes, so
/// a run starts where a multiply says it does and nothing is padded.
const TEXT_OFFSET_RUN: usize = 512;

/// Bytes at the front of a global dictionary index: the value count, the values a payload block
/// holds, the block count and the bits an offset is packed at.
const DICTIONARY_HEADER: usize = 16;

/// Set beside the offset width in the fourth word of a global dictionary index, meaning each
/// payload block says where in the file it starts and how long it is, rather than sitting directly
/// behind the block before it.
///
/// In that word rather than in a word of its own because the width is at most 32 and lives in a
/// `u32`, so the top of it has never been anything. A build old enough not to know the flag reads
/// the file's format before it reads any of this and refuses it there, and if it somehow did get
/// here it would find an offset width of two billion and say so.
///
/// The point of the flag is that a block written the moment it fills does not know what will be
/// written after it, so the payload of a column cannot be one run of bytes unless the whole column
/// is held until the file is closed. That is the memory the load cannot afford. What it costs is
/// eight bytes a block, against the block being a thousand values.
const DICTIONARY_SCATTERED: u32 = 1 << 31;
/// The dictionary index carries one four-byte substring signature per payload block.
const DICTIONARY_GRAMS: u32 = 1 << 30;
/// Each signature is [`TEXT_GRAM_BYTES`] long rather than the [`NARROW_GRAM_BYTES`] a format 28
/// file wrote.
const DICTIONARY_WIDE_GRAMS: u32 = 1 << 29;
/// Every flag the width word of a dictionary can carry above the offset width.
const DICTIONARY_FLAGS: u32 = DICTIONARY_SCATTERED | DICTIONARY_GRAMS | DICTIONARY_WIDE_GRAMS;

/// How many entries of a dictionary's sorted order sit in one block that is read and checked as a
/// unit.
///
/// Five hundred and twelve entries is between two and three kilobytes on the ClickBench string
/// columns, which is well under a page. A binary search over half a million entries makes nineteen
/// probes, and the first ten land in ten different blocks while the last nine land in the one block
/// that holds the answer, so the whole search reads about thirty kilobytes of a megabyte of order. A
/// smaller block would save a little on the early probes, cost a checksum and an end list four times
/// as long, and give the heads less to share a base with. A larger one would read more than it uses
/// on every probe.
const TEXT_RANK_BLOCK: usize = 512;

/// Bytes at the front of a rank block, which is the base of its heads and the width they are packed
/// at.
///
/// An entry used to be twelve bytes flat, eight for the head and four for the code, and on the five
/// ClickBench columns that have a dictionary worth the name that was 744 MB of a 12.2 GB file. Both
/// halves of it are nearly empty. The heads are the first eight bytes of the values in sorted order,
/// so a block of five hundred and twelve of them spans a tiny slice of the column, and on a column of
/// URLs they are all `http://w` and the block holds one distinct head. The codes are positions in a
/// dictionary of eighteen million, which is twenty five bits and not thirty two.
///
/// So a block now writes the smallest head in it, the bits the largest is above that, and the heads
/// and the codes packed at the width each needs. A block where every head agrees costs nine bytes
/// and the codes.
const RANK_BLOCK_HEADER: usize = size_of::<u64>() + 1;

impl NativeText {
    /// One block of the payload, read and decoded the first time anything asks for a value in it.
    ///
    /// The bytes handed back are the values of the block laid end to end, which is what the offsets
    /// describe, so a caller slices it with the offsets it already has. Where the block sits in the
    /// file is the only thing the caller cannot work out for itself, because the stored form is
    /// shorter than the decoded one and by a different amount in every block.
    fn payload_block(&self, block: usize) -> Result<Option<&[u8]>> {
        let Some(slot) = self.blocks.get(block) else { return Ok(None) };
        let bytes = slot.get_or_init(|| self.decode_block(block)).as_ref().map_err(Clone::clone)?;
        Ok(Some(bytes.as_slice()))
    }

    /// The character length of every value in one block, counted the first time it is asked for.
    ///
    /// The block is read out of [`Self::blocks`] where something already kept it and decoded and
    /// dropped where nothing did, so counting never adds a block to what this column holds. Two
    /// threads asking for the same block at once both count it and one of the two answers is kept,
    /// which costs a decode and is cheaper than a lock on every lookup.
    fn block_chars(&self, block: usize) -> Result<&[u32]> {
        let slot = self
            .char_lens
            .get(block)
            .ok_or_else(|| invalid("a block past the global dictionary"))?;
        if let Some(lens) = slot.get() {
            return Ok(lens);
        }
        let decoded;
        let bytes: &[u8] = match self.blocks.get(block).and_then(OnceLock::get) {
            Some(Ok(kept)) => kept,
            _ => {
                decoded = self.decode_block(block)?;
                &decoded
            }
        };
        let first = block * TEXT_PAYLOAD_VALUES;
        let last = (first + TEXT_PAYLOAD_VALUES).min(self.values);
        let ends = self.ends_within(first, last)?;
        if ends.len() != last - first {
            return Err(invalid("global dictionary offsets are short"));
        }
        let mut lens = Vec::with_capacity(ends.len());
        let mut start = u64::from(self.start_within(first)?);
        for &end in &ends {
            let value = usize::try_from(start)
                .ok()
                .zip(usize::try_from(end).ok())
                .and_then(|(from, to)| bytes.get(from..to))
                .ok_or_else(|| invalid("global dictionary value is past its block"))?;
            // A continuation byte of UTF-8 is `0b10xx_xxxx` and every other byte starts a
            // character, so the bytes that are not continuations are the characters.
            let characters = value.iter().filter(|byte| (**byte as i8) >= -0x40).count();
            lens.push(u32::try_from(characters).unwrap_or(u32::MAX));
            start = end;
        }
        Ok(slot.get_or_init(|| lens.into_boxed_slice()))
    }

    /// Reads and decodes one block of the payload, without deciding who keeps it.
    ///
    /// [`Self::payload_block`] keeps it forever, which is what a point read wants and what a walk
    /// of the whole dictionary must not do. Both call this and they differ in nothing else.
    fn decode_block(&self, block: usize) -> Result<Vec<u8>> {
        let len = self.lengths[block];
        let mut stored = vec![
            0;
            usize::try_from(len).map_err(|_| invalid(
                "global dictionary block does not fit in memory"
            ))?
        ];
        read_at(&self.file, self.starts[block], &mut stored)?;
        if checksum(&stored) != self.hashes[block] {
            return Err(invalid("global dictionary payload checksum differs"));
        }
        let first = block * TEXT_PAYLOAD_VALUES;
        let last = (first + TEXT_PAYLOAD_VALUES).min(self.values);
        let want = self.end_within(last - 1)? as usize;
        let values = string::decode_flat(&stored)?;
        if values.len() != last - first {
            return Err(invalid("global dictionary block holds the wrong value count"));
        }
        let bytes = values.into_bytes();
        if bytes.len() != want {
            return Err(invalid("global dictionary block decodes to the wrong length"));
        }
        Ok(bytes)
    }

    /// The block holding a value that a read hands over on loan, kept or decoded for the call.
    ///
    /// A block something already kept is read where it is. One nothing kept is kept the second
    /// time a loaned read decodes it while the column is holding less than [`Self::keep_budget`],
    /// and decoded into `decoded` and dropped with it otherwise, which is the policy
    /// [`TextSource::sweep`] explains. `scattered` is a read by code rather than in order, which
    /// stops dropping once it has dropped a column's worth of blocks, for the reason
    /// [`Self::visit_dropped`] gives.
    fn loaned_block<'a>(
        &'a self,
        block: usize,
        decoded: &'a mut Vec<u8>,
        scattered: bool,
    ) -> Result<&'a [u8]> {
        let kept = self.blocks.get(block).and_then(OnceLock::get);
        if let Some(Ok(kept)) = kept {
            return Ok(kept);
        }
        let again = kept.is_none()
            && self.swept.get(block).is_some_and(|swept| swept.swap(true, Atomic::Relaxed));
        let keep = again
            && (self.payload_kept.load(Atomic::Relaxed) < self.keep_budget
                || (scattered && self.visit_dropped.load(Atomic::Relaxed) >= self.blocks.len()));
        if keep {
            let kept = self
                .payload_block(block)?
                .ok_or_else(|| invalid("global dictionary block is past the payload"))?;
            self.payload_kept.fetch_add(kept.len(), Atomic::Relaxed);
            return Ok(kept);
        }
        *decoded = self.decode_block(block)?;
        if scattered && again {
            self.visit_dropped.fetch_add(1, Atomic::Relaxed);
        }
        Ok(decoded)
    }

    /// How many single offset reads make [`Self::value_ends`] worth building.
    ///
    /// As many reads as the dictionary has values. Building the table costs about thirty
    /// instructions a value once the fresh pages it lands in are counted, and a read out of it saves
    /// about thirty five, so it repays itself after roughly one read per value. The reads so far are
    /// the only guess there is at the reads to come, and waiting until they match the size of the
    /// dictionary is betting that a column read that much will be read that much again.
    ///
    /// A sixteenth was the first answer, from counting the unpacking alone at three instructions a
    /// value. ClickBench 38 showed what that missed: it reads about twenty thousand titles a
    /// statement out of a dictionary of three hundred and fifty thousand, crossed a sixteenth in its
    /// second statement and was two percent slower for a table it did not read enough to repay. A
    /// scan asking for the length of every row crosses it part way through its first statement on
    /// ClickBench, where a string column has about two rows for every value, and a filter that keeps
    /// a few thousand rows never does. The floor is there
    /// because a short dictionary would otherwise build a table for a handful of reads.
    fn ends_worth_unpacking(&self) -> usize {
        self.values.max(TEXT_PAYLOAD_VALUES)
    }

    /// The unpacked ends, if they are built or if this read is the one that makes them worth it.
    fn value_ends(&self) -> Option<&[u32]> {
        if let Some(built) = self.value_ends.get() {
            return built.as_deref();
        }
        if self.ends_asked.fetch_add(1, Atomic::Relaxed) < self.ends_worth_unpacking() {
            return None;
        }
        self.value_ends.get_or_init(|| self.unpack_ends()).as_deref()
    }

    /// Every end of the column, a run at a time.
    ///
    /// `None` rather than an error on anything wrong, because this is a cache in front of a reader
    /// that answers the same question. A column whose offsets are short or whose ends do not fit in
    /// four bytes gets no table and the same error it would have got, from the read that wanted it.
    fn unpack_ends(&self) -> Option<Vec<u32>> {
        let mut ends = vec![0u32; self.values];
        for (run, into) in ends.chunks_mut(TEXT_OFFSET_RUN).enumerate() {
            let bytes = self.packed().get(run * TEXT_OFFSET_RUN / 8 * self.offset_bits..)?;
            bitpack::unpack_tail_into(bytes, self.offset_bits, into, |bits| {
                u32::try_from(bits).unwrap_or(u32::MAX)
            })
            .ok()?;
        }
        // An end that did not fit was stored as the sentinel, and a real one cannot reach it because
        // a payload block is far smaller than four gigabytes. So the column keeps the packed reader.
        if ends.contains(&u32::MAX) { None } else { Some(ends) }
    }

    /// The packed offsets, which is the index past its header.
    fn packed(&self) -> &[u8] {
        self.offsets.get(DICTIONARY_HEADER..).unwrap_or_default()
    }

    /// Where the value at `index` ends inside its payload block.
    fn end_within(&self, index: usize) -> Result<u32> {
        if let Some(ends) = self.value_ends() {
            return ends
                .get(index)
                .copied()
                .ok_or_else(|| invalid("global dictionary offsets are short"));
        }
        let run = index / TEXT_OFFSET_RUN;
        let bytes = self
            .packed()
            .get(run * TEXT_OFFSET_RUN / 8 * self.offset_bits..)
            .ok_or_else(|| invalid("global dictionary offsets are short"))?;
        let end = bitpack::tail_at(bytes, self.offset_bits, index % TEXT_OFFSET_RUN)
            .map_err(|_| invalid("global dictionary offsets are short"))?;
        u32::try_from(end).map_err(|_| invalid("global dictionary offset is past the payload"))
    }

    /// Where every value in `first..last` ends inside its payload block, in one pass over the runs.
    ///
    /// [`Self::end_within`] answers for one value and pays for it twice over: it shifts a window to
    /// the bit the value starts at, and the copy that fills that window is a length the compiler does
    /// not know, so it is a call to `memcpy` rather than a load. A sweep asked for two of those per
    /// value, one for the end and one for the start that is the end before it, and on the ClickBench
    /// `URL` dictionary of eighteen million that was most of the half second a `LIKE` over it took.
    ///
    /// [`bitpack::unpack_tail_into`] walks the run instead, which makes the window a fixed width and
    /// so an unaligned load, and reads the bit position off a counter. A run is five hundred and
    /// twelve values and a block is two of them, so a block of a thousand and twenty four values
    /// costs two calls here and nothing per value.
    ///
    /// The answer is written straight into the result. A run that is wanted from its first value,
    /// which is every run but the one the sweep starts in, unpacks into its own window of the result
    /// and is never copied. Only a run joined part way through needs the scratch buffer, and there is
    /// at most one of those per sweep, so the buffer is allocated the first time one turns up.
    fn ends_within(&self, first: usize, last: usize) -> Result<Vec<u64>> {
        let mut ends = vec![0u64; last.saturating_sub(first)];
        let mut scratch = Vec::new();
        let mut at = first;
        while at < last {
            let run = at / TEXT_OFFSET_RUN;
            let stop = ((run + 1) * TEXT_OFFSET_RUN).min(last);
            let held = self.values.saturating_sub(run * TEXT_OFFSET_RUN).min(TEXT_OFFSET_RUN);
            let bytes = self
                .packed()
                .get(run * TEXT_OFFSET_RUN / 8 * self.offset_bits..)
                .ok_or_else(|| invalid("global dictionary offsets are short"))?;
            let from = at % TEXT_OFFSET_RUN;
            let upto = stop - run * TEXT_OFFSET_RUN;
            if upto > held || bytes.len() < bitpack::tail_len(held, self.offset_bits) {
                return Err(invalid("global dictionary offsets are short"));
            }
            let into = &mut ends[at - first..stop - first];
            if from == 0 {
                bitpack::unpack_tail_into(bytes, self.offset_bits, into, |bits| bits)
                    .map_err(|_| invalid("global dictionary offsets are short"))?;
            } else {
                scratch.resize(held, 0);
                bitpack::unpack_tail_into(bytes, self.offset_bits, &mut scratch, |bits| bits)
                    .map_err(|_| invalid("global dictionary offsets are short"))?;
                into.copy_from_slice(&scratch[from..upto]);
            }
            at = stop;
        }
        Ok(ends)
    }

    /// Where the value at `index` starts inside its payload block, which is where the value before
    /// it ended unless it is the first of the block.
    fn start_within(&self, index: usize) -> Result<u32> {
        if index % TEXT_PAYLOAD_VALUES == 0 { Ok(0) } else { self.end_within(index - 1) }
    }

    /// Where the value at `index` starts and ends inside its payload block.
    ///
    /// The two offsets sit next to each other in the same run unless the value opens one, and a run
    /// of seventeen bit offsets, which is what a block of a thousand strings needs, puts a pair of
    /// them inside one eight byte load. So the common case reads the packed bytes once rather than
    /// twice and does the bounds arithmetic once. This is asked once per string a text column hands
    /// out, and on ClickBench 27 the two reads together were a quarter of the query.
    fn span_within(&self, index: usize) -> Result<(u32, u32)> {
        if let Some(ends) = self.value_ends() {
            let end =
                *ends.get(index).ok_or_else(|| invalid("global dictionary offsets are short"))?;
            // The value before it in the same block, and zero where there is no value before it.
            // `index` is inside the table, so the one under it is too.
            let start = if index % TEXT_PAYLOAD_VALUES == 0 { 0 } else { ends[index - 1] };
            if start > end {
                return Err(invalid("global dictionary value ends before it starts"));
            }
            return Ok((start, end));
        }
        let within = index % TEXT_OFFSET_RUN;
        let (start, end) = if within == 0 {
            (self.start_within(index)?, self.end_within(index)?)
        } else {
            let run = index / TEXT_OFFSET_RUN;
            let bytes = self
                .packed()
                .get(run * TEXT_OFFSET_RUN / 8 * self.offset_bits..)
                .ok_or_else(|| invalid("global dictionary offsets are short"))?;
            let (start, end) = bitpack::tail_pair(bytes, self.offset_bits, within)
                .map_err(|_| invalid("global dictionary offsets are short"))?;
            let ends = u32::try_from(end)
                .map_err(|_| invalid("global dictionary offset is past the payload"))?;
            let starts = u32::try_from(start)
                .map_err(|_| invalid("global dictionary offset is past the payload"))?;
            (starts, ends)
        };
        if start > end {
            return Err(invalid("global dictionary value ends before it starts"));
        }
        Ok((start, end))
    }

    /// The block of the sorted order that holds `rank`, and where in it that rank sits.
    ///
    /// The block is read from the file and checked against the hash the index carries for it the
    /// first time anything asks, and kept after that, the same way a payload block is. A search
    /// makes about as many probes as the order has bits, so the whole search reads a handful of
    /// these and never the rest.
    fn rank_parts(&self, rank: usize) -> Result<(&[u8], usize)> {
        let slot = self
            .rank_blocks
            .get(rank / TEXT_RANK_BLOCK)
            .ok_or_else(|| invalid("global dictionary rank is past the order"))?;
        let block = slot
            .get_or_init(|| {
                let mut bytes = Vec::new();
                self.read_rank_block(rank / TEXT_RANK_BLOCK, &mut bytes)?;
                Ok(bytes)
            })
            .as_ref()
            .map_err(Clone::clone)?;
        Ok((block.as_slice(), rank % TEXT_RANK_BLOCK))
    }

    /// Reads block `which` of the sorted order into `bytes`, checked against the hash the index
    /// carries for it.
    fn read_rank_block(&self, which: usize, bytes: &mut Vec<u8>) -> Result<()> {
        let start = if which == 0 { 0 } else { self.rank_ends[which - 1] };
        let end = self.rank_ends[which];
        bytes.clear();
        bytes.resize((end - start) as usize, 0);
        read_at(&self.file, self.rank_at + start, bytes)?;
        let expected = self
            .rank_hashes
            .get(which)
            .ok_or_else(|| invalid("global dictionary rank block has no checksum"))?;
        if checksum(bytes) != *expected {
            return Err(invalid("global dictionary rank checksum differs"));
        }
        Ok(())
    }

    /// The first eight bytes of the value at `rank`, as the integer a comparison reads.
    fn head_at(&self, rank: usize) -> Result<u64> {
        let (block, within) = self.rank_parts(rank)?;
        let (base, width, packed) = rank_heads(block)?;
        let above = bitpack::tail_at(packed, width, within)
            .map_err(|_| invalid("global dictionary rank block is short of heads"))?;
        Ok(base.wrapping_add(above))
    }

    /// The packed codes of one rank block, which follow the heads on the next byte boundary.
    fn rank_codes<'block>(&self, block: &'block [u8], count: usize) -> Result<&'block [u8]> {
        let (_, width, packed) = rank_heads(block)?;
        packed
            .get(bitpack::tail_len(count, width)..)
            .ok_or_else(|| invalid("global dictionary rank block is short of codes"))
    }

    /// How many entries the block holding `rank` has, which is a full block except at the end.
    fn rank_block_len(&self, rank: usize) -> usize {
        let first = rank / TEXT_RANK_BLOCK * TEXT_RANK_BLOCK;
        TEXT_RANK_BLOCK.min(self.ranks - first)
    }
}

/// The base, the width and the packed bytes of one rank block's heads.
fn rank_heads(block: &[u8]) -> Result<(u64, usize, &[u8])> {
    let header = block
        .get(..RANK_BLOCK_HEADER)
        .ok_or_else(|| invalid("global dictionary rank block is short"))?;
    let base = u64::from_le_bytes(header[..8].try_into().expect("eight bytes"));
    let width = header[8] as usize;
    if width > 64 {
        return Err(invalid("global dictionary rank block packs heads past a word"));
    }
    Ok((base, width, &block[RANK_BLOCK_HEADER..]))
}

/// Bits one offset of a dictionary takes, which is what its widest payload block spans.
///
/// One width for the whole column rather than one a block. A block is 1,024 values of the same
/// column, so the blocks of a column are within a factor of two of each other on every ClickBench
/// string column, and a width a block would save a fraction of a bit and cost a byte a block plus
/// the arithmetic that finds where a block starts.
fn offset_width(ends: &[u32]) -> usize {
    // The ends are already relative to the block the value is in, so the last end of a block is that
    // block's total and the largest end anywhere is the widest block. There is no subtraction left
    // to do and no need to walk the blocks to find where one starts.
    let span = ends.iter().copied().max().unwrap_or(0);
    (u32::BITS - span.leading_zeros()) as usize
}

/// How many bytes `values` offsets take at `bits`, which is what the reader has to know before it
/// has read any of them.
fn offset_bytes(values: usize, bits: usize) -> usize {
    let full = values / TEXT_OFFSET_RUN;
    let rest = values % TEXT_OFFSET_RUN;
    full * TEXT_OFFSET_RUN / 8 * bits + bitpack::tail_len(rest, bits)
}

/// The end of every value within its payload block, packed a run at a time.
/// A run never straddles a block, because [`TEXT_OFFSET_RUN`] divides [`TEXT_PAYLOAD_VALUES`], which
/// is what lets this be a walk of the ends rather than arithmetic against a per block base.
fn encode_offsets(ends: &[u32], bits: usize, out: &mut Vec<u8>) -> Result<()> {
    let mut run = Vec::with_capacity(TEXT_OFFSET_RUN);
    for chunk in ends.chunks(TEXT_OFFSET_RUN) {
        run.clear();
        run.extend(chunk.iter().map(|&end| u64::from(end)));
        bitpack::pack_tail(&run, bits, out)
            .map_err(|_| invalid("global dictionary offsets do not pack"))?;
    }
    Ok(())
}

/// How many bits a code of a dictionary of `values` entries takes.
fn code_width(values: usize) -> usize {
    match u64::try_from(values).unwrap_or(u64::MAX) {
        0 | 1 => 0,
        last => (u64::BITS - (last - 1).leading_zeros()) as usize,
    }
}

impl TextSource for NativeText {
    fn len(&self) -> usize {
        self.values
    }

    fn might_contain(&self, first: usize, literal: &[u8]) -> Result<bool> {
        let Some(grams) = &self.grams else { return Ok(true) };
        if literal.len() < 4 || first >= self.values {
            return Ok(true);
        }
        let verdict = grams.verdicts(&self.file, literal)?;
        Ok(verdict.get(first / TEXT_PAYLOAD_VALUES).copied().unwrap_or(true))
    }

    fn bytes_at(&self, index: usize) -> Result<Option<&[u8]>> {
        if index >= self.values {
            return Ok(None);
        }
        let (start, end) = self.span_within(index)?;
        if start == end {
            return Ok(Some(&[]));
        }
        // A block holds a fixed number of values rather than a fixed number of bytes, so the value
        // is in one block and the offsets already say where in it.
        let block = index / TEXT_PAYLOAD_VALUES;
        let Some(bytes) = self.payload_block(block)? else { return Ok(None) };
        Ok(bytes.get(start as usize..end as usize))
    }

    fn bytes_len_at(&self, index: usize) -> Result<Option<usize>> {
        if index >= self.values {
            return Ok(None);
        }
        let (start, end) = self.span_within(index)?;
        Ok(Some((end - start) as usize))
    }

    /// Every length out of the unpacked ends in one loop, which is the point of having them.
    ///
    /// The whole run of positions counts towards [`Self::ends_worth_unpacking`] at once, because a
    /// caller asking for a vector of lengths has said how many it wants, and a vector of them is
    /// usually enough on its own. Until the table is worth building this is the row at a time read,
    /// the same as the default.
    fn bytes_lens_at(&self, indices: &[u32], into: &mut Vec<i64>) -> Result<()> {
        self.ends_asked.fetch_add(indices.len(), Atomic::Relaxed);
        into.reserve(indices.len());
        let Some(ends) = self.value_ends() else {
            for &index in indices {
                into.push(
                    self.bytes_len_at(index as usize)?
                        .map_or(0, |len| i64::try_from(len).unwrap_or(i64::MAX)),
                );
            }
            return Ok(());
        };
        if let Some(lens) = self.value_lens.get_or_init(|| lengths_of(ends)) {
            lens.extend_at(indices, into);
            return Ok(());
        }
        for &index in indices {
            let index = index as usize;
            // Past the end is no value and so no length, which is what a row at a time read says.
            let Some(&end) = ends.get(index) else {
                into.push(0);
                continue;
            };
            let start = if index % TEXT_PAYLOAD_VALUES == 0 { 0 } else { ends[index - 1] };
            if start > end {
                return Err(invalid("global dictionary value ends before it starts"));
            }
            into.push(i64::from(end - start));
        }
        Ok(())
    }

    /// Every length in characters out of the counts kept a block at a time, which is what keeps a
    /// scan of `length` from holding the column decoded. See [`NativeText::char_lens`].
    fn chars_lens_at(&self, indices: &[u32], into: &mut Vec<i64>) -> Result<()> {
        into.reserve(indices.len());
        for &index in indices {
            let index = index as usize;
            // Past the end is no value and so no length, which is what a row at a time read says.
            if index >= self.values {
                into.push(0);
                continue;
            }
            let lens = self.block_chars(index / TEXT_PAYLOAD_VALUES)?;
            let len = lens
                .get(index % TEXT_PAYLOAD_VALUES)
                .ok_or_else(|| invalid("global dictionary block holds the wrong value count"))?;
            into.push(i64::from(*len));
        }
        Ok(())
    }

    /// The rest of the block holding `first`, decoded into a buffer that may die with the call.
    ///
    /// A block is the unit this format decodes, so a walk that wants every value is going to decode
    /// every block whatever it does. The question is whether it keeps them, and both answers are
    /// wrong on their own. [`Self::payload_block`] keeps every block it is asked for, so a reader
    /// that walked the whole dictionary through `bytes_at` ended up holding the whole dictionary
    /// decoded, 4.2 GB on ClickBench `URL`. Keeping none of them makes the next statement asking
    /// the same question decode all of it again, which on the same column at a million rows is a
    /// `LIKE` going from 2.7 ms to 16.2 ms.
    ///
    /// So a sweep keeps what it decodes for the second time while the column is under
    /// [`TEXT_KEEP_BUDGET`] and drops it after that. A block already in hand is used where it is there and costs nothing either way.
    fn sweep(
        &self,
        first: usize,
        limit: usize,
        body: &mut dyn FnMut(usize, &[u8]) -> Result<()>,
    ) -> Result<usize> {
        let limit = limit.min(self.values);
        if first >= limit {
            return Ok(first);
        }
        let block = first / TEXT_PAYLOAD_VALUES;
        let last = ((block + 1) * TEXT_PAYLOAD_VALUES).min(limit);
        let mut decoded = Vec::new();
        let bytes = self.loaned_block(block, &mut decoded, false)?;
        let ends = self.ends_within(first, last)?;
        if ends.len() != last - first {
            return Err(invalid("global dictionary offsets are short"));
        }
        let mut start = u64::from(self.start_within(first)?);
        // row at a time: the caller is handed one value after another, and what it does with one is
        // its own business, so there is no shape here for anything but a walk.
        for (index, &end) in (first..last).zip(&ends) {
            let value = usize::try_from(start)
                .ok()
                .zip(usize::try_from(end).ok())
                .and_then(|(from, to)| bytes.get(from..to))
                .ok_or_else(|| invalid("global dictionary value is past its block"))?;
            body(index, value)?;
            start = end;
        }
        Ok(last)
    }

    /// The values at `indices` a block at a time, each block read once for the call.
    ///
    /// The positions are put in code order first, because the codes of a vector are in row order
    /// and land all over the dictionary, and read in that order each block a vector touches would
    /// be looked up once for every row in it. Whether a block is kept is
    /// [`NativeText::loaned_block`]'s decision, which keeps at most the budget of this column
    /// until the reads have shown they come back to the same blocks too often for dropping them to
    /// be cheap.
    fn visit_at(
        &self,
        indices: &[u32],
        body: &mut dyn FnMut(usize, &[u8]) -> Result<()>,
    ) -> Result<()> {
        let mut order = (0..indices.len()).collect::<Vec<_>>();
        order.sort_unstable_by_key(|&at| indices[at]);
        let block_of = |at: usize| {
            let index = indices[at] as usize;
            (index < self.values).then_some(index / TEXT_PAYLOAD_VALUES)
        };
        let mut decoded = Vec::new();
        let mut run = 0;
        while run < order.len() {
            let Some(block) = block_of(order[run]) else {
                // Past the end is no value, and every position after this one is past it too.
                for &at in &order[run..] {
                    body(at, &[])?;
                }
                break;
            };
            let upto = run + order[run..].partition_point(|&at| block_of(at) == Some(block));
            let bytes = self.loaned_block(block, &mut decoded, true)?;
            for &at in &order[run..upto] {
                let (start, end) = self.span_within(indices[at] as usize)?;
                let value = bytes
                    .get(start as usize..end as usize)
                    .ok_or_else(|| invalid("global dictionary value is past its block"))?;
                body(at, value)?;
            }
            run = upto;
        }
        Ok(())
    }

    /// Each block the indices land in, decoded once and dropped, or read where it is already kept.
    ///
    /// Never kept, unlike [`Self::sweep`] under its budget, because a scattered read is a one off:
    /// a synopsis turned into values is turned once and remembered by the reader as values, a few
    /// kilobytes, where the blocks it went through are megabytes nobody asks for again.
    fn visit(
        &self,
        indices: &[usize],
        body: &mut dyn FnMut(usize, &[u8]) -> Result<()>,
    ) -> Result<()> {
        let mut at = 0;
        while at < indices.len() {
            let block = indices[at] / TEXT_PAYLOAD_VALUES;
            let upto =
                at + indices[at..].partition_point(|&index| index / TEXT_PAYLOAD_VALUES == block);
            let wanted = &indices[at..upto];
            if wanted.iter().any(|&index| index >= self.values) {
                return Err(invalid("a visited value is past the global dictionary"));
            }
            let decoded;
            let bytes: &[u8] = match self.blocks.get(block).and_then(OnceLock::get) {
                Some(Ok(kept)) => kept,
                _ => {
                    decoded = self.decode_block(block)?;
                    &decoded
                }
            };
            for (offset, &index) in wanted.iter().enumerate() {
                let (start, end) = self.span_within(index)?;
                let value = bytes
                    .get(start as usize..end as usize)
                    .ok_or_else(|| invalid("global dictionary value is past its block"))?;
                body(at + offset, value)?;
            }
            at = upto;
        }
        Ok(())
    }

    fn ranks(&self) -> Option<usize> {
        (self.ranks > 0).then_some(self.ranks)
    }

    /// The boundary for `wanted`, out of [`Self::searched`] where it is there and put there where
    /// it is not.
    ///
    /// The lock is held over the search rather than dropped and taken again, so that two threads
    /// asking for the same value at the same time do the work once between them. That is the shape
    /// the scan actually arrives in: sixteen instances of a top N, all reading the same column, all
    /// improving their bound over the same early chunks.
    fn below(&self, ranks: usize, wanted: &[u8]) -> Result<(usize, bool)> {
        let mut memo = self.searched.lock().map_err(|_| invalid("a poisoned dictionary search"))?;
        if let Some(&answer) = memo.get(wanted) {
            return Ok(answer);
        }
        let answer = search_below(self, ranks, wanted)?;
        if memo.len() >= TEXT_SEARCH_MEMO {
            memo.clear();
        }
        memo.insert(wanted.to_vec(), answer);
        Ok(answer)
    }

    fn compare_rank(&self, rank: usize, wanted: &[u8]) -> Result<Ordering> {
        // The head settles the probe unless the two values start with the same eight bytes, and
        // only then is a value read. On a column of URLs that is the difference between a search
        // that touches one block of the payload and a search that touches nineteen of them.
        let settled = self.head_at(rank)?.cmp(&head(wanted));
        if settled != Ordering::Equal {
            return Ok(settled);
        }
        let code = self.code_at_rank(rank)?;
        let bytes = self
            .bytes_at(code as usize)?
            .ok_or_else(|| invalid("global dictionary order names a code it does not have"))?;
        Ok(bytes.cmp(wanted))
    }

    fn code_at_rank(&self, rank: usize) -> Result<u32> {
        let (block, within) = self.rank_parts(rank)?;
        let codes = self.rank_codes(block, self.rank_block_len(rank))?;
        let code = bitpack::tail_at(codes, self.code_bits, within)
            .map_err(|_| invalid("global dictionary rank block is short of codes"))?;
        let code = u32::try_from(code)
            .map_err(|_| invalid("global dictionary order names a code it does not have"))?;
        if code as usize >= self.len() {
            return Err(invalid("global dictionary order names a code it does not have"));
        }
        Ok(code)
    }

    fn code_ranks(&self) -> Option<&[u32]> {
        // The order is a permutation of the positions, so inverting it needs every position to be
        // named exactly once. Anything else and the slice would have holes, and a caller indexing
        // it by a code would read a rank that belongs to nothing.
        if self.ranks == 0 || self.ranks != self.len() {
            return None;
        }
        self.code_ranks
            .get_or_init(|| {
                let mut ranks = vec![u32::MAX; self.ranks];
                // A block at a time rather than a rank at a time, because reading it per rank pays
                // for the bounds check, the division and the lock on every one of them.
                //
                // A block nothing has read yet is read into one buffer that is reused, rather than
                // through `rank_parts`, which would keep every block of the order once this is
                // done with it. The inverse is all anything wants after this, and on the `Referer`
                // column of the ClickBench file the blocks are tens of megabytes held for nothing.
                let mut scratch = Vec::new();
                let mut codes = vec![0u64; TEXT_RANK_BLOCK];
                for first in (0..self.ranks).step_by(TEXT_RANK_BLOCK) {
                    let which = first / TEXT_RANK_BLOCK;
                    let block = match self.rank_blocks.get(which)?.get() {
                        Some(kept) => kept.as_ref().ok()?.as_slice(),
                        None => {
                            self.read_rank_block(which, &mut scratch).ok()?;
                            scratch.as_slice()
                        }
                    };
                    let count = self.rank_block_len(first);
                    let packed = self.rank_codes(block, count).ok()?;
                    let codes = codes.get_mut(..count)?;
                    bitpack::unpack_tail_into(packed, self.code_bits, codes, |bits| bits).ok()?;
                    for (within, &code) in codes.iter().enumerate() {
                        let code = usize::try_from(code).ok()?;
                        *ranks.get_mut(code)? = u32::try_from(first + within).ok()?;
                    }
                }
                if ranks.contains(&u32::MAX) {
                    return None;
                }
                Some(ranks)
            })
            .as_deref()
    }

    fn footprint(&self) -> usize {
        self.offsets.capacity()
            + self
                .value_ends
                .get()
                .and_then(Option::as_ref)
                .map_or(0, |ends| ends.capacity() * size_of::<u32>())
            + self.value_lens.get().and_then(Option::as_ref).map_or(0, Lengths::footprint)
            + self
                .code_ranks
                .get()
                .and_then(Option::as_ref)
                .map_or(0, |ranks| ranks.capacity() * size_of::<u32>())
            + self.rank_hashes.capacity() * size_of::<u64>()
            + self.rank_ends.capacity() * size_of::<u64>()
            + self.rank_blocks.capacity() * size_of::<OnceLock<Result<Vec<u8>>>>()
            + self
                .rank_blocks
                .iter()
                .filter_map(OnceLock::get)
                .filter_map(|result| result.as_ref().ok())
                .map(Vec::capacity)
                .sum::<usize>()
            + self.blocks.capacity() * size_of::<OnceLock<Result<Vec<u8>>>>()
            + self.char_lens.capacity() * size_of::<OnceLock<Box<[u32]>>>()
            + self
                .char_lens
                .iter()
                .filter_map(OnceLock::get)
                .map(|lens| lens.len() * size_of::<u32>())
                .sum::<usize>()
            + self.hashes.capacity() * size_of::<u64>()
            + self.starts.capacity() * size_of::<u64>()
            + self.lengths.capacity() * size_of::<u64>()
            + self.grams.as_ref().map_or(0, NativeGrams::footprint)
            + self
                .blocks
                .iter()
                .filter_map(OnceLock::get)
                .filter_map(|result| result.as_ref().ok())
                .map(Vec::capacity)
                .sum::<usize>()
    }
}

/// Every table wide part number in order, with the stripe it belongs to.
fn places(table: &Table) -> Result<Vec<Place>> {
    let mut places = Vec::with_capacity(table.stripes.len().saturating_mul(STRIPE_PARTS));
    for (at, stripe) in table.stripes.iter().enumerate() {
        let index = u32::try_from(at).map_err(|_| invalid("too many stripes"))?;
        for (part, &rows) in stripe.parts.iter().enumerate() {
            places.push(Place {
                stripe: index,
                part: u32::try_from(part).map_err(|_| invalid("too many parts in a stripe"))?,
                rows,
            });
        }
    }
    Ok(places)
}

/// Reads one column's section of a stripe's index page.
///
/// The section carries its own checksum, so a reader that wants one column out of a hundred and
/// five preads a few hundred bytes and still knows that what it got is what was written.
fn read_index<F: Positional + ?Sized>(
    file: &F,
    stripe: &Stripe,
    column: usize,
) -> Result<Vec<PartSpan>> {
    let page = stripe.pages.get(column).ok_or_else(|| invalid("stripe page is missing"))?;
    read_index_span(file, stripe.index, *page, stripe.parts.len(), column)
}

fn read_index_span<F: Positional + ?Sized>(
    file: &F,
    index: Span,
    page: Span,
    parts: usize,
    column: usize,
) -> Result<Vec<PartSpan>> {
    let section = index_section(parts)?;
    let at = column.checked_mul(section).ok_or_else(|| invalid("index page offset overflow"))?;
    let end = at.checked_add(section).ok_or_else(|| invalid("index page offset overflow"))?;
    if end > index.length as usize {
        return Err(invalid("index page is shorter than its columns"));
    }
    let mut bytes = vec![0; section];
    let offset =
        index.offset.checked_add(at as u64).ok_or_else(|| invalid("index page offset overflow"))?;
    read_at(file, offset, &mut bytes)?;
    let entries = section - size_of::<u64>();
    let stored = u64::from_le_bytes(bytes[entries..].try_into().expect("eight bytes"));
    if checksum(&bytes[..entries]) != stored {
        // With where it was read from, because the two ways this fires look identical from the
        // message alone: a file somebody damaged, and a file we wrote to the wrong offset.
        return Err(invalid(&format!(
            "index page section checksum differs, column {column} of {parts} parts at {offset}, \
             wanted {stored:016x} and got {:016x}",
            checksum(&bytes[..entries]),
        )));
    }
    let mut spans = Vec::with_capacity(parts);
    let mut start = 0_usize;
    for part in 0..parts {
        let at = part * INDEX_ENTRY;
        let length = u32::from_le_bytes(bytes[at..at + 4].try_into().expect("four bytes")) as usize;
        let hash = u64::from_le_bytes(bytes[at + 4..at + 12].try_into().expect("eight bytes"));
        spans.push(PartSpan { start, length, hash });
        start = start.checked_add(length).ok_or_else(|| invalid("column page length overflow"))?;
    }
    if start != page.length as usize {
        return Err(invalid("column page length differs from its index"));
    }
    Ok(spans)
}

/// One part's bytes out of a whole column page.
fn part_bytes(page: &[u8], span: PartSpan) -> Result<&[u8]> {
    let end = span.start.checked_add(span.length).ok_or_else(|| invalid("part range overflow"))?;
    page.get(span.start..end).ok_or_else(|| invalid("part exceeds its column page"))
}

/// Puts one stripe of one column in the cache, and hands back the page for the pool to count when
/// it is a page the column did not already hold.
///
/// The index goes in its own slot and stays. Only the page is under the budget, and the pool is
/// what enforces it, once the caller has let go of the column's lock.
fn remember(cached: &mut Cached, held: &CachedColumn) -> Option<(usize, Arc<AtomicBool>)> {
    if let Some(slot) = cached.index.get_mut(held.stripe) {
        if slot.is_none() {
            *slot = Some(Arc::clone(&held.index));
        }
    }
    let page = held.page.clone()?;
    let slot = cached.pages.get_mut(held.stripe)?;
    if slot.is_some() {
        return None;
    }
    let bytes = page.bytes.len();
    // Set, so that the page a worker has just paid to read is not the one the pass it pays for
    // lets go of before the worker has read a part out of it.
    let used = Arc::new(AtomicBool::new(true));
    *slot = Some(Resident { page, used: Arc::clone(&used) });
    Some((bytes, used))
}

/// Every table a native file holds, without the directory of any of them.
///
/// This is what opening a database reads. It is the small level of the directory, so the cost is
/// proportional to how many tables there are rather than to how much data they hold, and a session
/// that touches two tables of eight decodes two table directories.
///
/// The file handle is shared with every reader this hands out. Eight tables in one file is one open
/// file descriptor, not eight, which is the other thing one file buys over a file per table.
#[derive(Debug, Clone)]
pub struct Catalog {
    file: Arc<File>,
    size: u64,
    entries: Arc<Vec<Entry>>,
    /// The views the file holds, whole, since a view has no second level to read later.
    views: Arc<Vec<ViewEntry>>,
    opening: Opening,
    /// Where every reader this hands out counts its pages.
    pool: PagePool,
}

/// Signed integer sums and non-null counts for selected columns, plus total table rows.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CertifiedSums {
    pub columns: Vec<(i128, u64)>,
    pub rows: u64,
}

/// Exact ends of an integer or date column, including a certified all-null column.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IntegerExtremes {
    Null,
    Values { low: i128, high: i128 },
}

/// A complete numeric value-to-row-count synopsis; `None` represents SQL NULL.
pub type NumericFrequencies = Vec<(Option<i128>, u64)>;

impl Catalog {
    /// Reads the highest valid catalog slot and nothing under it.
    ///
    /// The readers it hands out keep pages in a pool of their own with no budget, so each column
    /// holds its floor of four stripes and no more. A database opens with [`Catalog::open_in`].
    ///
    /// # Errors
    ///
    /// If the file has no valid committed catalog or a catalog pointer is out of bounds.
    pub fn open(path: impl AsRef<Path>) -> Result<Self> {
        Self::open_in(path, &PagePool::default())
    }

    /// The same, with every reader it hands out keeping its pages in `pool`.
    ///
    /// # Errors
    ///
    /// If the file has no valid committed catalog or a catalog pointer is out of bounds.
    pub fn open_in(path: impl AsRef<Path>, pool: &PagePool) -> Result<Self> {
        let (file, size, _, bytes, opening) = slot_bytes(path)?;
        let (entries, views) = decode_catalog(&bytes, size)?;
        Ok(Self {
            file: Arc::new(file),
            size,
            entries: Arc::new(entries),
            views: Arc::new(views),
            opening,
            pool: pool.clone(),
        })
    }

    /// The tables in the file, in the order they were written.
    pub fn names(&self) -> impl ExactSizeIterator<Item = &str> {
        self.entries.iter().map(|entry| entry.name.as_str())
    }

    /// The same tables with how many rows each of them holds.
    ///
    /// The names alone answer which tables the file has, which is what a checkpoint needs to know.
    /// A load asks a second question: whether a table already in the file is really in the way of
    /// the one it wants to write. A table with no rows is not, because it has no pages the next
    /// generation would have to carry, so the count has to come out of the catalog beside the name.
    pub fn rows(&self) -> impl ExactSizeIterator<Item = (&str, usize)> {
        self.entries.iter().map(|entry| (entry.name.as_str(), entry.rows))
    }

    /// The views in the file, in the order they were written.
    ///
    /// Whole, unlike [`Catalog::names`], which hands back names and makes the caller ask for a table
    /// by one. A view is a few strings and a column list and it was all read at open, so there is
    /// nothing left to go and fetch and no reason to make the caller ask twice.
    pub fn views(&self) -> impl ExactSizeIterator<Item = &ViewEntry> {
        self.views.iter()
    }

    /// How many tables the file holds.
    #[must_use]
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    /// Whether the file holds no table at all, which is what [`Writer::empty`] writes and what a
    /// database somebody dropped the last table out of comes back as.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// Opens one table by name, decoding its directory now.
    ///
    /// # Errors
    ///
    /// If there is no table by that name, or its directory is torn or points outside the file.
    pub fn table(&self, name: &str) -> Result<Reader> {
        let entry = self
            .entries
            .iter()
            .find(|entry| entry.name == name)
            .ok_or_else(|| invalid(&format!("the file holds no table called {name}")))?;
        // Checked and then decoded a window at a time, so that the directory's own bytes are never
        // all in memory beside the table they decode into. It is read twice, and the second read
        // comes out of the page cache the first one filled.
        let (offset, length) = (entry.directory.offset, entry.directory.length as usize);
        if file_checksum(&self.file, offset, length)? != entry.directory.hash {
            return Err(invalid(&format!("the directory of table {name} does not checksum")));
        }
        let mut opening = self.opening;
        opening.reads += 1;
        opening.bytes += u64::from(entry.directory.length);
        Reader::build(
            Arc::clone(&self.file),
            self.size,
            read_directory(Cursor::over(&self.file, offset, length), self.size, Some(offset))?,
            u64::from(entry.directory.length),
            opening,
            self.pool.clone(),
        )
    }

    /// Counts one signed integer column from its encoded parts without building metadata for
    /// unrelated columns. The counts are computed from row encodings when this is called.
    /// Nullable and non-cascade parts use the ordinary decoder for that part.
    ///
    /// # Errors
    ///
    /// If the directory, selected page index, checksum, or encoded integer is invalid.
    pub fn integer_tally(&self, name: &str, column: usize) -> Result<Option<Vec<(i64, u64)>>> {
        let mut counts = BTreeMap::<i64, u64>::new();
        let Some(()) = self.integer_fold(name, column, |value, count| {
            let held = counts.entry(value).or_default();
            *held = held.checked_add(count).ok_or_else(|| invalid("integer count overflow"))?;
            Ok(())
        })?
        else {
            return Ok(None);
        };
        Ok(Some(counts.into_iter().collect()))
    }

    /// Visits a signed integer column's row values without building per-part or table-wide count
    /// maps. The caller combines the emitted counts for its query at runtime.
    ///
    /// # Errors
    ///
    /// If the selected file data is invalid or the callback rejects a count.
    pub fn integer_fold(
        &self,
        name: &str,
        column: usize,
        mut emit: impl FnMut(i64, u64) -> Result<()>,
    ) -> Result<Option<()>> {
        let entry = self
            .entries
            .iter()
            .find(|entry| entry.name == name)
            .ok_or_else(|| invalid(&format!("the file holds no table called {name}")))?;
        let field =
            entry.fields.get(column).ok_or_else(|| invalid("integer column index out of range"))?;
        if !signed_integer(&field.ty) {
            return Ok(None);
        }
        let (offset, length) = (entry.directory.offset, entry.directory.length as usize);
        if file_checksum(&self.file, offset, length)? != entry.directory.hash {
            return Err(invalid(&format!("the directory of table {name} does not checksum")));
        }
        quick_integer_fold(
            &self.file,
            Cursor::over(&self.file, offset, length),
            entry,
            self.size,
            column,
            &mut emit,
        )?;
        Ok(Some(()))
    }

    /// Counts non-null, nonzero values from generic column frequencies when complete. For an
    /// older file or a partial catalog synopsis, reads the validated native directory without
    /// building a reader for every stripe. Returns `None` when the bounded frequency synopsis
    /// cannot prove the count, so callers can use the ordinary query path.
    pub fn nonzero_count(&self, name: &str, column: usize) -> Result<Option<u64>> {
        let entry = self
            .entries
            .iter()
            .find(|entry| entry.name == name)
            .ok_or_else(|| invalid(&format!("the file holds no table called {name}")))?;
        let Some(field) = entry.fields.get(column) else {
            return Err(invalid("frequency column index out of range"));
        };
        if !matches!(
            field.ty,
            LogicalType::TinyInt
                | LogicalType::SmallInt
                | LogicalType::Integer
                | LogicalType::BigInt
                | LogicalType::UTinyInt
                | LogicalType::USmallInt
                | LogicalType::UInteger
                | LogicalType::UBigInt
        ) {
            return Ok(None);
        }
        let (offset, length) = (entry.directory.offset, entry.directory.length as usize);
        if file_checksum(&self.file, offset, length)? != entry.directory.hash {
            return Err(invalid(&format!("the directory of table {name} does not checksum")));
        }
        if let Some(Some(frequencies)) = entry.frequencies.get(column) {
            return frequencies
                .iter()
                .filter(|(value, _)| value.is_some_and(|value| value != 0))
                .try_fold(0_u64, |total, (_, count)| total.checked_add(*count))
                .map(Some)
                .ok_or_else(|| invalid("numeric frequency count overflow"));
        }
        quick_nonzero(
            Cursor::over(&self.file, offset, length),
            &entry.name,
            &entry.fields,
            entry.rows,
            column,
        )
    }

    /// Exact signed-integer sums and non-null counts from the small catalog. The table directory
    /// checksum is still checked once before any certificate can answer a query.
    pub fn aggregate_sums(&self, name: &str, columns: &[usize]) -> Result<Option<CertifiedSums>> {
        let entry = self
            .entries
            .iter()
            .find(|entry| entry.name == name)
            .ok_or_else(|| invalid(&format!("the file holds no table called {name}")))?;
        let mut sums = Vec::with_capacity(columns.len());
        for &column in columns {
            let Some(field) = entry.fields.get(column) else {
                return Err(invalid("aggregate column index out of range"));
            };
            if !signed_integer(&field.ty) {
                return Ok(None);
            }
            let Some(sum) = entry.aggregates[column] else {
                return Ok(None);
            };
            sums.push(sum);
        }
        let (offset, length) = (entry.directory.offset, entry.directory.length as usize);
        if file_checksum(&self.file, offset, length)? != entry.directory.hash {
            return Err(invalid(&format!("the directory of table {name} does not checksum")));
        }
        Ok(Some(CertifiedSums { columns: sums, rows: entry.rows as u64 }))
    }

    /// Exact non-null distinct count from the small catalog, after checking the table directory.
    pub fn distinct_count(&self, name: &str, column: usize) -> Result<Option<u64>> {
        let entry = self
            .entries
            .iter()
            .find(|entry| entry.name == name)
            .ok_or_else(|| invalid(&format!("the file holds no table called {name}")))?;
        let Some(count) = entry.distincts.get(column).copied() else {
            return Err(invalid("distinct column index out of range"));
        };
        let Some(count) = count else { return Ok(None) };
        let (offset, length) = (entry.directory.offset, entry.directory.length as usize);
        if file_checksum(&self.file, offset, length)? != entry.directory.hash {
            return Err(invalid(&format!("the directory of table {name} does not checksum")));
        }
        Ok(Some(count))
    }

    /// Exact integer or date ends from the small catalog after checking the table directory.
    pub fn integer_extremes(&self, name: &str, column: usize) -> Result<Option<IntegerExtremes>> {
        let entry = self
            .entries
            .iter()
            .find(|entry| entry.name == name)
            .ok_or_else(|| invalid(&format!("the file holds no table called {name}")))?;
        let Some(extremes) = entry.extremes.get(column).copied() else {
            return Err(invalid("extremes column index out of range"));
        };
        let Some(extremes) = extremes else { return Ok(None) };
        let (offset, length) = (entry.directory.offset, entry.directory.length as usize);
        if file_checksum(&self.file, offset, length)? != entry.directory.hash {
            return Err(invalid(&format!("the directory of table {name} does not checksum")));
        }
        Ok(Some(match extremes {
            None => IntegerExtremes::Null,
            Some((low, high)) => IntegerExtremes::Values { low, high },
        }))
    }

    /// Complete numeric frequencies from the small catalog, after checking the table directory.
    pub fn exact_numeric_frequencies(
        &self,
        name: &str,
        column: usize,
    ) -> Result<Option<NumericFrequencies>> {
        let entry = self
            .entries
            .iter()
            .find(|entry| entry.name == name)
            .ok_or_else(|| invalid(&format!("the file holds no table called {name}")))?;
        let Some(frequencies) = entry.frequencies.get(column).cloned() else {
            return Err(invalid("numeric frequency column index out of range"));
        };
        let Some(frequencies) = frequencies else { return Ok(None) };
        let (offset, length) = (entry.directory.offset, entry.directory.length as usize);
        if file_checksum(&self.file, offset, length)? != entry.directory.hash {
            return Err(invalid(&format!("the directory of table {name} does not checksum")));
        }
        Ok(Some(frequencies))
    }

    /// The schema copied into the small file catalog, available without opening the table directory.
    pub fn table_fields(&self, name: &str) -> Option<&[Field]> {
        self.entries.iter().find(|entry| entry.name == name).map(|entry| entry.fields.as_slice())
    }
}

/// Where the slot naming `generation` goes, which is the one the generation before it did not use.
///
/// Generation 1 takes the slot at 16, so a file written once is byte for byte the file this wrote
/// before there was a second generation to write.
fn slot_offset(generation: u64) -> u64 {
    16 + (generation - 1) % 2 * SLOT_BYTES as u64
}

/// The header and the bytes the highest valid slot points at.
///
/// Both levels of the directory are reached this way, so the magic check, the version check and the
/// choice between the two slots live here rather than being written out twice.
fn slot_bytes(path: impl AsRef<Path>) -> Result<(File, u64, Slot, Vec<u8>, Opening)> {
    let file = File::open(path).map_err(io)?;
    let size = file.metadata().map_err(io)?.len();
    let (slot, bytes, opening) = committed_slot(&file, size)?;
    Ok((file, size, slot, bytes, opening))
}

/// The committed slot of a file that is `size` bytes long, and the catalog it points at.
///
/// The half of [`slot_bytes`] that does not care how the file was opened. A reader comes here with
/// the `std::fs::File` it goes on to share between its threads, and a writer with the `rudb_io`
/// file it is about to append to.
fn committed_slot<F: Positional + ?Sized>(file: &F, size: u64) -> Result<(Slot, Vec<u8>, Opening)> {
    if size < HEADER {
        return Err(invalid("file is shorter than its header"));
    }
    let mut header = [0; HEADER as usize];
    read_at(file, 0, &mut header)?;
    let mut opening = Opening { reads: 1, bytes: HEADER };
    let version = u32::from_le_bytes([header[8], header[9], header[10], header[11]]);
    // The two halves are worth telling apart. A wrong magic is a file that was never ours and
    // the answer is to look at the path. A wrong version is our own file from another build,
    // and the number this build wants is the only thing that tells the reader whether to
    // rebuild the file or to go back to the binary that wrote it.
    if &header[..8] != MAGIC {
        return Err(invalid("the header does not begin with a rudb native magic"));
    }
    if !READABLE.contains(&version) {
        return Err(invalid(&format!(
            "the file is format {version} and this build reads format {FORMAT}, so it has to \
                 be written again"
        )));
    }
    let mut selected = None;
    for start in [16, 16 + SLOT_BYTES] {
        let slot = Slot::read(&header[start..start + SLOT_BYTES]);
        if slot.generation == 0 || slot.length == 0 || slot.length as usize > MAX_DIRECTORY {
            continue;
        }
        let Some(end) = slot.offset.checked_add(u64::from(slot.length)) else { continue };
        if slot.offset < HEADER || end > size {
            continue;
        }
        let mut bytes = vec![0; slot.length as usize];
        read_at(file, slot.offset, &mut bytes)?;
        opening.reads += 1;
        opening.bytes += u64::from(slot.length);
        if checksum(&bytes) == slot.hash
            && selected
                .as_ref()
                .is_none_or(|(old, _): &(Slot, Vec<u8>)| old.generation < slot.generation)
        {
            selected = Some((slot, bytes));
        }
    }
    let (slot, bytes) = selected.ok_or_else(|| invalid("no committed directory slot is valid"))?;
    Ok((slot, bytes, opening))
}

impl Reader {
    /// Opens a file that holds exactly one table.
    ///
    /// # Errors
    ///
    /// If the file has no valid committed directory, a directory pointer is out of bounds, or the
    /// file holds more than one table, which is a file that has to be opened by name.
    pub fn open(path: impl AsRef<Path>) -> Result<Self> {
        let catalog = Catalog::open(path)?;
        let mut names = catalog.names();
        let name = names.next().ok_or_else(|| invalid("the file holds no table"))?.to_string();
        if names.next().is_some() {
            return Err(invalid(
                "the file holds more than one table, so it has to be opened by name",
            ));
        }
        catalog.table(&name)
    }

    /// Builds a reader over one decoded table directory.
    fn build(
        file: Arc<File>,
        size: u64,
        table: Table,
        directory: u64,
        opening: Opening,
        pool: PagePool,
    ) -> Result<Self> {
        let places = places(&table)?;
        let dictionaries = (0..table.fields.len()).map(|_| OnceLock::new()).collect();
        let table_fields = table.fields.len();
        let stripes = table.stripes.len();
        let columns = (0..table.fields.len())
            .map(|_| {
                Mutex::new(Cached {
                    pages: (0..stripes).map(|_| None).collect(),
                    index: (0..stripes).map(|_| None).collect(),
                    seen: vec![false; stripes],
                    ..Cached::default()
                })
            })
            .collect::<Vec<_>>();
        let cache = Shelf {
            columns,
            held: (0..table_fields).map(|_| AtomicUsize::new(0)).collect(),
            kept: AtomicUsize::new(CACHED_STRIPES_PER_COLUMN),
        };
        let sieves: Vec<Vec<SieveSlot>> = (0..table.fields.len())
            .map(|_| table.stripes.iter().map(|_| OnceLock::new()).collect())
            .collect();
        let part_ranges: Vec<Vec<RangeSlot>> = (0..table.fields.len())
            .map(|_| table.stripes.iter().map(|_| OnceLock::new()).collect())
            .collect();
        Ok(Self {
            file,
            table: Arc::new(table),
            dictionaries: Arc::new(dictionaries),
            loading: Arc::new((0..table_fields).map(|_| Mutex::new(())).collect()),
            frequency_values: Arc::new((0..table_fields).map(|_| OnceLock::new()).collect()),
            frequency_summaries: Arc::new((0..table_fields).map(|_| OnceLock::new()).collect()),
            opened: Arc::new(AtomicUsize::new(0)),
            sieves: Arc::new(sieves),
            part_ranges: Arc::new(part_ranges),
            places: Arc::new(places),
            cache: Arc::new(cache),
            pool,
            pages: Arc::new(AtomicUsize::new(0)),
            indexes: Arc::new(AtomicUsize::new(0)),
            size,
            directory,
            opening,
        })
    }

    /// What this reader has read so far, and what opening it cost.
    ///
    /// Public because the claim of `spec/stats/04-in-memory.md` section 4.2 is about this number
    /// and a claim nobody can check is a comment. A caller that wants to know whether opening a
    /// file touched the data asks here, and gets an answer that does not depend on what the page
    /// cache happened to hold.
    #[must_use]
    pub fn reads(&self) -> Reads {
        Reads {
            opening: self.opening,
            pages: self.pages.load(Atomic::Relaxed),
            indexes: self.indexes.load(Atomic::Relaxed),
            dictionaries: self.opened.load(Atomic::Relaxed),
        }
    }

    /// Where the file's bytes went, from the directory alone.
    ///
    /// No page is read, so this costs the same on a 45 GB table as on an empty one. See [`Layout`]
    /// for what is charged where and for why the three things that are not columns stay separate.
    #[must_use]
    pub fn layout(&self) -> Layout {
        let table = &self.table;
        let stripes = table.stripes.as_slice();
        let columns = table
            .fields
            .iter()
            .enumerate()
            .map(|(at, field)| ColumnLayout {
                name: field.name.clone(),
                kind: field.ty.to_string(),
                pages: sum(stripes.iter().map(|stripe| span_bytes(&stripe.pages, at))),
                memberships: sum(stripes.iter().map(|stripe| stripe.memberships.bytes(at))),
                sieves: sum(stripes.iter().map(|stripe| stripe.sieves.bytes(at))),
                part_ranges: sum(stripes.iter().map(|stripe| stripe.part_ranges.bytes(at))),
                dictionary: dictionary_bytes(table, at),
            })
            .collect();
        Layout {
            file: self.size,
            rows: table.rows,
            stripes: stripes.len(),
            parts: self.places.len(),
            columns,
            indexes: sum(stripes.iter().map(|stripe| u64::from(stripe.index.length))),
            directory: self.directory,
            header: HEADER,
        }
    }

    /// What every part of one column is stored as, which is what `pragma_storage_info` reports.
    ///
    /// Unlike [`Self::layout`] this reads the data, because the encoder's choice is in the page and
    /// nowhere else. The directory says how many bytes a column took and says nothing about what
    /// shape they are in, and the shape is the question worth asking: the same rows in a different
    /// order come back bit packed on one file and plain on another, and that is the difference a
    /// clustered load makes to a scan.
    ///
    /// One read per stripe rather than one per part. A part is a few kilobytes out of a page that
    /// is a quarter of a megabyte, so asking part by part would read the same page sixty four
    /// times. Nothing is put in the page cache, because a caller asking what a file looks like is
    /// not about to scan it and evicting the pages a real query wants would be a poor trade.
    ///
    /// # Errors
    ///
    /// If the column is outside the schema, or a page, index section or checksum is invalid.
    pub fn stored(&self, column: usize) -> Result<Vec<StoredPart>> {
        let field = self
            .table
            .fields
            .get(column)
            .ok_or_else(|| invalid("stored column index out of range"))?;
        let mut stored = Vec::with_capacity(self.places.len());
        let mut row = 0;
        for (at, stripe) in self.table.stripes.iter().enumerate() {
            let page = stripe.pages.get(column).ok_or_else(|| invalid("stripe page is missing"))?;
            let index = read_index(&self.file, stripe, column)?;
            let mut bytes = vec![0; page.length as usize];
            read_at(&self.file, page.offset, &mut bytes)?;
            let ranges = self.stripe_part_ranges(at, column);
            for (part, &rows) in stripe.parts.iter().enumerate() {
                let span = *index.get(part).ok_or_else(|| invalid("part index out of range"))?;
                let held = part_bytes(&bytes, span)?;
                let range = ranges.and_then(|held| held.get(part));
                stored.push(StoredPart {
                    stripe: at,
                    part,
                    row,
                    rows: rows as usize,
                    encoding: page_encoding(&field.ty, rows as usize, held),
                    bytes: span.length as u64,
                    page: page.offset,
                    offset: span.start as u64,
                    low: range
                        .and_then(|range| range.low.clone())
                        .and_then(|bound| bound.into_value(&field.ty)),
                    high: range
                        .and_then(|range| range.high.clone())
                        .and_then(|bound| bound.into_value(&field.ty)),
                    nulls: range.map(|range| range.nulls),
                });
                row += rows as usize;
            }
        }
        Ok(stored)
    }

    /// How many parts the table has, which is how many chunks a scan of it reads.
    #[must_use]
    pub fn parts(&self) -> usize {
        self.places.len()
    }

    /// The parts of each stripe, in table wide part numbers.
    ///
    /// A scan that wants one worker to own the page it reads hands work out in these runs. The
    /// stripes are contiguous in part numbering and all but the last hold sixty four parts, but a
    /// stripe can be flushed early when rows arrive out of order, so the runs are read off the
    /// directory rather than worked out from a constant.
    #[must_use]
    pub fn stripe_parts(&self) -> Vec<std::ops::Range<usize>> {
        let mut runs = Vec::with_capacity(self.table.stripes.len());
        let mut start = 0;
        for stripe in &self.table.stripes {
            let end = start + stripe.parts.len();
            runs.push(start..end);
            start = end;
        }
        runs
    }

    /// How many rows one stripe holds, in the numbering [`Self::stripe_parts`] hands back.
    ///
    /// Off the directory, which is already in memory, rather than by the caller asking for each
    /// part in turn through the catalog. Nothing past the end holds any rows.
    #[must_use]
    pub fn stripe_rows(&self, stripe: usize) -> usize {
        self.table.stripes.get(stripe).map_or(0, |held| held.rows)
    }

    /// Asks the page cache to keep `stripes` stripes of every column instead of the default.
    ///
    /// This only ever raises the number. A scan that gives each worker a whole stripe has one page
    /// per column per worker open at once, and a cache smaller than that is worse than no cache at
    /// all: every worker's page is evicted by the others before it has finished its stripe, so it
    /// reads a quarter of a megabyte for every part it takes out of it.
    pub fn keep_stripes(&self, stripes: usize) {
        self.cache.kept.fetch_max(stripes, Atomic::Relaxed);
    }

    /// Rows in one part, or zero when the part number is past the table.
    #[must_use]
    pub fn part_rows(&self, at: usize) -> usize {
        self.places.get(at).map_or(0, |place| place.rows as usize)
    }

    /// The committed table directory.
    #[must_use]
    pub fn table(&self) -> &Table {
        &self.table
    }

    /// Exact leading frequencies when the stored synopsis proves a count-descending prefix.
    ///
    /// The returned list can be longer than `top`. Keeping the stored tail lets a later TopN apply
    /// additional ordering keys without losing a value tied with the requested boundary.
    ///
    /// # Errors
    ///
    /// If the column is outside the schema or a stored value does not fit its declared type.
    pub fn top_frequencies(&self, column: usize, top: usize) -> Result<Option<Vec<(Value, u64)>>> {
        let field = self
            .table
            .fields
            .get(column)
            .ok_or_else(|| invalid("frequency column index out of range"))?;
        let Some(summary) = self.frequency_summary(column)? else {
            return Ok(None);
        };
        if top == 0 || summary.entries.len() < top {
            return Ok(None);
        }
        let boundary = summary.entries[top - 1].count;
        if boundary <= summary.omitted_max {
            return Ok(None);
        }
        self.decode_frequencies(column, &field.ty, &summary.entries).map(Some)
    }

    /// Exact leading counts for a numeric key paired with a stable-dictionary string key.
    ///
    /// The stored prefix is returned only when its requested boundary strictly beats the bound on
    /// every pair omitted at load time. The returned tail may be longer than `top`, as with
    /// [`Self::top_frequencies`], so downstream ordering can settle ties without reading rows.
    ///
    /// # Errors
    ///
    /// If either column is outside the schema or persisted pair metadata is inconsistent with the
    /// frequency synopsis or dictionary it names.
    pub fn top_pair_frequencies(
        &self,
        first: usize,
        second: usize,
        top: usize,
    ) -> Result<Option<PairFrequencyCounts>> {
        if first >= self.table.fields.len() || second >= self.table.fields.len() {
            return Err(invalid("pair frequency column index out of range"));
        }
        let Some(summary) =
            self.table.pair_frequencies.iter().find(|summary| {
                summary.first as usize == first && summary.second as usize == second
            })
        else {
            return Ok(None);
        };
        if top == 0 || summary.entries.len() < top {
            return Ok(None);
        }
        let boundary = summary.entries[top - 1].count;
        if boundary <= summary.omitted_max {
            return Ok(None);
        }
        let first_summary = self
            .frequency_summary(first)?
            .ok_or_else(|| invalid("pair frequency first column has no synopsis"))?;
        let anchors = self
            .decode_frequencies(first, &self.table.fields[first].ty, &first_summary.entries)?
            .into_iter()
            .map(|(value, _)| value)
            .collect::<Vec<_>>();
        let dictionary = self
            .dictionary(second)?
            .ok_or_else(|| invalid("pair frequency second column has no dictionary"))?;
        let mut codes = summary.entries.iter().filter_map(|entry| entry.second).collect::<Vec<_>>();
        codes.sort_unstable();
        codes.dedup();
        let texts = dictionary
            .try_values_visited(&codes.iter().map(|&code| code as usize).collect::<Vec<_>>())?;
        let mut out = Vec::with_capacity(summary.entries.len());
        for entry in &summary.entries {
            if entry.count < boundary {
                break;
            }
            let first = anchors
                .get(entry.first_entry as usize)
                .cloned()
                .ok_or_else(|| invalid("pair frequency anchor is outside its values"))?;
            let second = match entry.second {
                None => Value::Null,
                Some(code) => {
                    let at = codes
                        .binary_search(&code)
                        .map_err(|_| invalid("pair frequency code was not among the codes read"))?;
                    texts[at].clone()
                }
            };
            out.push((vec![first, second], entry.count));
        }
        Ok(Some(out))
    }

    /// Every value of one column with the number of rows holding it, when the synopsis is complete.
    ///
    /// The heavy hitter pass keeps a bounded set of candidates and decrements them all when it runs
    /// out of room, so what it usually ends with is the leading values and a bound on everything it
    /// dropped. `omitted_max` of zero says that never happened: no candidate was ever decremented and
    /// the entries did not overflow the stored budget, so the list is every distinct value of the
    /// column with an exact count, and a null counts as a value of its own rather than being skipped.
    ///
    /// That makes a whole class of question answerable without reading a row. How many rows hold a
    /// value, how many do not, and what a `GROUP BY` of that column with a count over it produces are
    /// all in here. It is only ever true of a column with few enough distinct values, which is the
    /// case worth having, because that is exactly the column a grouping or an equality filter would
    /// otherwise walk every row to answer.
    ///
    /// `None` when the column has no synopsis, or has one that dropped anything.
    ///
    /// # Errors
    ///
    /// If the column is outside the schema or a stored value does not fit its declared type.
    pub fn exact_frequencies(&self, column: usize) -> Result<Option<Vec<(Value, u64)>>> {
        let Some(prefix) = self.frequency_prefix(column)? else {
            return Ok(None);
        };
        Ok((prefix.omitted_max == 0).then_some(prefix.entries))
    }

    /// Every value the synopsis lists with the number of rows holding it, and a bound on the rest.
    ///
    /// The counts are exact whether or not the list is complete. The heavy hitter pass keeps a
    /// bounded candidate set and then recounts only the candidates that survived it, so a value that
    /// made it into the list carries the number of rows that really hold it rather than whatever the
    /// pass had left over. What the pass loses is values, not counts.
    ///
    /// `omitted_max` is how many rows the most common value left out can hold, and zero says nothing
    /// was left out at all, which is what [`exact_frequencies`] asks for. Above zero the list is the
    /// leading values of the column and everything else is somewhere between no rows and that bound.
    ///
    /// That prefix is worth reading on its own. A column with a value in half its rows and a long
    /// tail behind it has no complete synopsis and never will, and it is the column where dividing
    /// the rows by the distinct count is furthest from the truth.
    ///
    /// `None` when the column has no synopsis.
    ///
    /// # Errors
    ///
    /// If the column is outside the schema or a stored value does not fit its declared type.
    ///
    /// [`exact_frequencies`]: Self::exact_frequencies
    pub fn frequency_prefix(&self, column: usize) -> Result<Option<FrequencyPrefix>> {
        let field = self
            .table
            .fields
            .get(column)
            .ok_or_else(|| invalid("frequency column index out of range"))?;
        let Some(summary) = self.frequency_summary(column)? else {
            return Ok(None);
        };
        let entries = self.decode_frequencies(column, &field.ty, &summary.entries)?;
        Ok(Some(FrequencyPrefix { entries, omitted_max: summary.omitted_max }))
    }

    /// One column's synopsis, read back from the file when the directory left it there.
    fn frequency_summary(&self, column: usize) -> Result<Option<Cow<'_, FrequencySummary>>> {
        Ok(match self.table.frequencies.get(column) {
            None | Some(None) => None,
            Some(Some(Frequencies::Held(summary))) => Some(Cow::Borrowed(summary)),
            Some(Some(Frequencies::Stored { span, values })) => {
                let slot = self
                    .frequency_summaries
                    .get(column)
                    .ok_or_else(|| invalid("frequency column index out of range"))?;
                if let Some(summary) = slot.get() {
                    return Ok(Some(Cow::Borrowed(summary.as_ref())));
                }
                let field = self
                    .table
                    .fields
                    .get(column)
                    .ok_or_else(|| invalid("frequency column index out of range"))?;
                let mut bytes = vec![0; span.length as usize];
                read_at(&self.file, span.offset, &mut bytes)?;
                let summary =
                    decode_summary(&mut Cursor::new(&bytes), field, self.table.rows, *values)?;
                let summary = summary.ok_or_else(|| invalid("a stored synopsis is missing"))?;
                let _ = slot.set(Arc::new(summary));
                Some(Cow::Borrowed(slot.get().expect("the decoded summary was stored").as_ref()))
            }
        })
    }

    /// Turns stored frequency entries into values of the column's own type.
    ///
    /// Remembered per column, because the planner asks once for every estimate that touches the
    /// column and the executor asks again, and the answer is a few hundred values. The codes of a
    /// string column are read through [`Vector::try_values_visited`], which does not keep the blocks
    /// it decodes, so what a query answered out of the synopsis holds is those values and not the
    /// hundred or so dictionary blocks they are scattered over.
    fn decode_frequencies(
        &self,
        column: usize,
        ty: &LogicalType,
        entries: &[FrequencyEntry],
    ) -> Result<Vec<(Value, u64)>> {
        if let Some(values) = self.frequency_values.get(column).and_then(OnceLock::get) {
            return Ok(values.as_ref().clone());
        }
        let values = self.decode_frequencies_once(column, ty, entries)?;
        if let Some(slot) = self.frequency_values.get(column) {
            let _ = slot.set(Arc::new(values.clone()));
        }
        Ok(values)
    }

    fn decode_frequencies_once(
        &self,
        column: usize,
        ty: &LogicalType,
        entries: &[FrequencyEntry],
    ) -> Result<Vec<(Value, u64)>> {
        let stored_texts = self.table.frequency_texts.get(column).filter(|texts| !texts.is_empty());
        if stored_texts.is_some_and(|texts| texts.len() != entries.len()) {
            return Err(invalid("frequency text count differs from its synopsis"));
        }
        let dictionary =
            if coded_type(ty) && stored_texts.is_none() { self.dictionary(column)? } else { None };
        let mut codes = entries
            .iter()
            .filter_map(|entry| match entry.value {
                FrequencyValue::Code(code) => Some(code as usize),
                _ => None,
            })
            .collect::<Vec<_>>();
        codes.sort_unstable();
        codes.dedup();
        let texts = match &dictionary {
            Some(dictionary) if !codes.is_empty() => dictionary.try_values_visited(&codes)?,
            _ => Vec::new(),
        };
        let mut out = Vec::with_capacity(entries.len());
        for (entry_at, entry) in entries.iter().enumerate() {
            let value = match entry.value {
                FrequencyValue::Null => {
                    if stored_texts.and_then(|texts| texts[entry_at].as_ref()).is_some() {
                        return Err(invalid("a null frequency entry has text"));
                    }
                    Value::Null
                }
                FrequencyValue::Integer(value) => match *ty {
                    LogicalType::TinyInt => Value::TinyInt(
                        i8::try_from(value)
                            .map_err(|_| invalid("frequency TINYINT is out of range"))?,
                    ),
                    LogicalType::UTinyInt => Value::UTinyInt(
                        u8::try_from(value)
                            .map_err(|_| invalid("frequency UTINYINT is out of range"))?,
                    ),
                    LogicalType::USmallInt => Value::USmallInt(
                        u16::try_from(value)
                            .map_err(|_| invalid("frequency USMALLINT is out of range"))?,
                    ),
                    LogicalType::UInteger => Value::UInteger(
                        u32::try_from(value)
                            .map_err(|_| invalid("frequency UINTEGER is out of range"))?,
                    ),
                    LogicalType::UBigInt => Value::UBigInt(
                        u64::try_from(value)
                            .map_err(|_| invalid("frequency UBIGINT is out of range"))?,
                    ),
                    LogicalType::SmallInt => Value::SmallInt(
                        i16::try_from(value)
                            .map_err(|_| invalid("frequency SMALLINT is out of range"))?,
                    ),
                    LogicalType::Integer => Value::Integer(
                        i32::try_from(value)
                            .map_err(|_| invalid("frequency INTEGER is out of range"))?,
                    ),
                    LogicalType::BigInt => Value::BigInt(
                        i64::try_from(value)
                            .map_err(|_| invalid("frequency BIGINT is out of range"))?,
                    ),
                    LogicalType::Date => Value::Date(
                        i32::try_from(value)
                            .map_err(|_| invalid("frequency DATE is out of range"))?,
                    ),
                    LogicalType::Timestamp => Value::Timestamp(
                        i64::try_from(value)
                            .map_err(|_| invalid("frequency TIMESTAMP is out of range"))?,
                    ),
                    _ => return Err(invalid("integer frequency belongs to another type")),
                },
                FrequencyValue::Code(code) => {
                    if let Some(text) = stored_texts.and_then(|texts| texts[entry_at].as_ref()) {
                        if *ty == LogicalType::Blob {
                            Value::Blob(text.clone())
                        } else {
                            Value::Varchar(
                                String::from_utf8(text.clone())
                                    .map_err(|_| invalid("frequency text is not UTF-8"))?,
                            )
                        }
                    } else {
                        if dictionary.is_none() {
                            return Err(invalid("frequency code has no dictionary or stored text"));
                        }
                        let at = codes
                            .binary_search(&(code as usize))
                            .map_err(|_| invalid("frequency code was not among the codes read"))?;
                        texts[at].clone()
                    }
                }
            };
            out.push((value, entry.count));
        }
        Ok(out)
    }

    /// Sparse rows belonging to the bounded numeric frequency candidate set.
    ///
    /// The list is omitted when collecting it would exceed the fixed storage budget. A composite
    /// aggregate may accept a result over these rows only when its requested boundary is strictly
    /// greater than `omitted_max`.
    ///
    /// # Errors
    ///
    /// If the column is outside the schema.
    pub fn frequency_occurrences(&self, column: usize) -> Result<Option<FrequencyOccurrences>> {
        let field = self
            .table
            .fields
            .get(column)
            .ok_or_else(|| invalid("frequency column index out of range"))?;
        let Some(summary) = self.frequency_summary(column)? else {
            return Ok(None);
        };
        if summary.ordinals.is_empty() {
            return Ok(None);
        }
        let (anchors, anchor_indices) = if summary.ordinal_entries.len() == summary.ordinals.len() {
            let entries = self.decode_frequencies(column, &field.ty, &summary.entries)?;
            (entries.into_iter().map(|(value, _)| value).collect(), summary.ordinal_entries.clone())
        } else {
            (Vec::new(), Vec::new())
        };
        Ok(Some(FrequencyOccurrences {
            omitted_max: summary.omitted_max,
            ordinals: summary.ordinals.clone(),
            anchors,
            anchor_indices,
        }))
    }

    /// How many distinct values one column holds, counting a null as no value.
    ///
    /// A string column of this format is written against one dictionary that covers the whole table.
    /// A code is handed out the first time a value is seen and nothing ever removes one, so the
    /// number of codes is the number of distinct values exactly rather than an estimate. That makes
    /// `COUNT(DISTINCT column)` over a whole table a question the directory already knows the answer
    /// to, and the alternative is a hash table with a row per distinct value built from a pass over
    /// every row.
    ///
    /// A null in the column used to make this `None` and no longer does. A null row is written as
    /// the code for the empty string, so a nullable column's dictionary can hold an empty string
    /// that no row of it actually has, and the dictionary on its own does not say which case it is.
    /// The writer does know, because it counts the non-null rows that use each code on its way to
    /// the frequency summary, so it records how many codes any row holds and the directory carries
    /// that number. This reads it rather than the size of the dictionary, which also means the
    /// dictionary page is not opened to answer.
    ///
    /// An integer column has no dictionary, and its count comes from the set the writer keeps on its
    /// numeric frequency pass instead, which is exact up to a cap. `None` for a column past that cap
    /// and for every column that is neither, where a sketch would answer approximately and SQL asked
    /// for the exact number.
    ///
    /// # Errors
    ///
    /// If the column is outside the schema.
    pub fn distinct_values(&self, column: usize) -> Result<Option<u64>> {
        self.table
            .distincts
            .get(column)
            .copied()
            .ok_or_else(|| invalid("distinct column index out of range"))
    }

    /// How many rows of one column are null, added up over the stripes.
    ///
    /// Every stripe records this exactly when it is written, because a null count is not a bound
    /// that is allowed to be wide the way a minimum and a maximum are: a filter that reads one too
    /// many is slow and a `COUNT` that reads one too many is wrong. Adding up a few hundred numbers
    /// already in memory is what makes `COUNT(column)` over a whole table free.
    ///
    /// # Errors
    ///
    /// If the column is outside the schema.
    pub fn null_count(&self, column: usize) -> Result<u64> {
        if column >= self.table.fields.len() {
            return Err(invalid("null count column index out of range"));
        }
        let mut nulls = 0_u64;
        for stripe in &self.table.stripes {
            let range = stripe
                .zone
                .column(column)
                .ok_or_else(|| invalid("stripe zone is narrower than the schema"))?;
            nulls = nulls
                .checked_add(range.nulls as u64)
                .ok_or_else(|| invalid("null count overflow"))?;
        }
        Ok(nulls)
    }

    /// The smallest and the largest value of one string column, from the order beside its values.
    ///
    /// The dictionary holds exactly the values the column holds, so the first and the last of them
    /// in sorted order are the column's minimum and maximum. Two reads of a rank block settle what
    /// otherwise walks a million rows.
    ///
    /// `None` when the column is not a string, when the file was written before version 9 and so has
    /// no order, when the column has no values at all, or when it has a null in it, which is the
    /// placeholder again: the empty string a null is written as would sort ahead of every real
    /// value and be reported as the minimum.
    ///
    /// # Errors
    ///
    /// If the column is outside the schema, or a rank names a code the dictionary does not have.
    pub fn text_extremes(&self, column: usize) -> Result<Option<(Value, Value)>> {
        if self.null_count(column)? > 0 || self.demoted(column) {
            return Ok(None);
        }
        let Some(dictionary) = self.dictionary(column)? else { return Ok(None) };
        let Some(ranks) = dictionary.ranks() else { return Ok(None) };
        if ranks == 0 {
            return Ok(None);
        }
        let low = text_at_rank(&dictionary, 0)?;
        let high = text_at_rank(&dictionary, ranks - 1)?;
        Ok(Some((low, high)))
    }

    /// The smallest and the largest value of one column, when every stripe wrote exact ends.
    ///
    /// A stripe's ends are allowed to be wider than the truth, because a bound that rules out a
    /// chunk that could not match is still correct when it rules out nothing. That is what makes
    /// them cheap to write for a bit packed or a dictionary column, and it is also what stops them
    /// answering a `MIN`. So each stripe says which of the two it wrote, and this answers only when
    /// all of them walked their rows.
    ///
    /// `None` for a column with no ends, for an empty table, and for a column any stripe of which
    /// guessed. Nulls need no special case, because the ends skip them the same way `MIN` does.
    ///
    /// One case is given up on that did not have to be. A stripe merges the ends of its sixty four
    /// parts, and a part with no ends at all erases the merged ones, because a part whose rows are
    /// not covered by the stripe's ends is a stripe that would skip rows it should keep. A part of
    /// nothing but nulls has no rows to cover and so did not need to erase anything, but the merge
    /// cannot tell that part from a part whose layout it could not read. So a column with a chunk
    /// of nothing but nulls in the middle of it goes and reads the rows. That is slow and right,
    /// and the fix is a row count per part rather than anything here.
    ///
    /// # Errors
    ///
    /// If the column is outside the schema.
    pub fn exact_extremes(&self, column: usize) -> Result<Option<(Bound, Bound)>> {
        if column >= self.table.fields.len() {
            return Err(invalid("extremes column index out of range"));
        }
        let mut low: Option<Bound> = None;
        let mut high: Option<Bound> = None;
        for stripe in &self.table.stripes {
            let range = stripe
                .zone
                .column(column)
                .ok_or_else(|| invalid("stripe zone is narrower than the schema"))?;
            if !range.exact {
                return Ok(None);
            }
            // A stripe of nothing but nulls has no ends and says nothing about the column's, which
            // is why this skips it rather than giving up on the whole column. A stripe that has
            // rows and still has no end is a layout whose values this cannot see, and skipping that
            // one would answer with an end taken from the other stripes, so it gives up instead.
            let (Some(small), Some(large)) = (range.low.as_ref(), range.high.as_ref()) else {
                if stripe.rows > range.nulls {
                    return Ok(None);
                }
                continue;
            };
            low = Some(low.map_or_else(|| small.clone(), |held| held.smaller(small.clone())));
            high = Some(high.map_or_else(|| large.clone(), |held| held.larger(large.clone())));
        }
        Ok(low.zip(high))
    }

    /// The sum of one integer column and how many rows went into it, when every stripe wrote one.
    ///
    /// The count beside the sum is the non-null rows, because that is what a `SUM` adds up and what
    /// an `AVG` divides by, and a caller that had to work it out from the row count and the null
    /// count would be doing the same walk twice.
    ///
    /// `None` for anything that is not an integer column, for a file written by something that did
    /// not record it, and when adding the stripes together would overflow.
    ///
    /// # Errors
    ///
    /// If the column is outside the schema.
    pub fn exact_sum(&self, column: usize) -> Result<Option<(i128, u64)>> {
        if column >= self.table.fields.len() {
            return Err(invalid("sum column index out of range"));
        }
        let mut total = 0_i128;
        let mut rows = 0_u64;
        for stripe in &self.table.stripes {
            let range = stripe
                .zone
                .column(column)
                .ok_or_else(|| invalid("stripe zone is narrower than the schema"))?;
            let Some(part) = range.sum else { return Ok(None) };
            let Some(sum) = total.checked_add(part) else { return Ok(None) };
            total = sum;
            rows = rows.saturating_add(stripe.rows as u64 - range.nulls as u64);
        }
        Ok(Some((total, rows)))
    }

    /// Certified host groups over a string column, when the caller's inclusive row-count bound
    /// excludes every host the synopsis omitted.
    pub fn host_groups(
        &self,
        column: usize,
        minimum_count: u64,
    ) -> Result<Option<Vec<host::HostEntry>>> {
        if column >= self.table.fields.len() {
            return Err(invalid("host group column index out of range"));
        }
        let Some(summary) = &self.table.host_groups else { return Ok(None) };
        if summary.column != column || minimum_count <= summary.omitted_max {
            return Ok(None);
        }
        Ok(Some(summary.entries.clone()))
    }

    /// Whether the column's dictionary stopped taking values partway through the load, and so
    /// decodes the stripes written before that and says nothing about the column as a whole. See
    /// `DEMOTED`.
    #[must_use]
    pub fn demoted(&self, column: usize) -> bool {
        self.table.demoted.get(column).copied().unwrap_or(false)
    }

    /// The global dictionary of a column, opened once however many workers ask for it at once.
    ///
    /// The unlocked look is first because it is the answer every time after the first and it costs a
    /// load. Everybody who misses it queues on [`Self::loading`] and looks again on the way in, so
    /// the one who arrived first does the reading and the rest take what it left. Waiting is the
    /// cheaper thing to do: the work behind the lock is a page read, a checksum and the decode of a
    /// dictionary that can hold half a million entries, and the alternative is every worker of the
    /// scan doing all of it and all but one dropping the result on the floor.
    fn dictionary(&self, column: usize) -> Result<Option<Arc<Vector>>> {
        let Some(page) = self.table.dictionaries[column] else { return Ok(None) };
        if let Some(dictionary) = self.dictionaries[column].get() {
            return Ok(Some(Arc::clone(dictionary)));
        }
        let _queued = self.loading[column].lock().map_err(|_| invalid("a poisoned dictionary"))?;
        if let Some(dictionary) = self.dictionaries[column].get() {
            return Ok(Some(Arc::clone(dictionary)));
        }
        self.opened.fetch_add(1, Atomic::Relaxed);
        let dictionary = Arc::new(open_global_dictionary(
            Arc::clone(&self.file),
            page,
            &self.table.fields[column].ty,
            TEXT_KEEP_BUDGET,
        )?);
        let _ = self.dictionaries[column].set(Arc::clone(&dictionary));
        Ok(Some(dictionary))
    }

    /// Reads one section's extent table and checks it against the entry that names it.
    ///
    /// # Errors
    ///
    /// If the entry points outside the file, the table does not checksum, or it does not decode as
    /// a run of extents in element order.
    pub fn extents(&self, of: &Section) -> Result<Vec<section::Extent>> {
        if of.extent_bytes == 0 {
            return Ok(Vec::new());
        }
        let mut bytes = vec![0; of.extent_bytes as usize];
        read_at(&self.file, of.extent_page, &mut bytes)?;
        if checksum(&bytes) != of.hash {
            return Err(invalid("a section's extent table does not checksum"));
        }
        let extents = section::decode_extents(&bytes)?;
        if extents.len() != of.extents as usize {
            return Err(invalid("a section's extent table is not the length the entry says"));
        }
        Ok(extents)
    }

    /// Reads and verifies one extent of a section.
    ///
    /// This is what section 3.2's second rule is for. A reduction that needs one extent of a two
    /// gigabyte forward link reads and checksums that extent and nothing else, which is the whole
    /// difference between a structure that works at SF100 and issue #745.
    ///
    /// # Errors
    ///
    /// If the extent points outside the file, or its bytes do not checksum.
    pub fn extent(&self, of: &section::Extent) -> Result<Vec<u8>> {
        let end = of
            .offset
            .checked_add(u64::from(of.length))
            .ok_or_else(|| invalid("an extent overflows the file"))?;
        if of.offset < HEADER || end > self.size {
            return Err(invalid("an extent is outside the file"));
        }
        let mut bytes = vec![0; of.length as usize];
        read_at(&self.file, of.offset, &mut bytes)?;
        if checksum(&bytes) != of.hash {
            return Err(invalid("an extent does not checksum"));
        }
        Ok(bytes)
    }

    /// Reads a whole section's payload, every extent of it, in order.
    ///
    /// For a structure that is resident anyway, which a key map is. Anything large enough that the
    /// split matters should be walking [`Reader::extents`] and taking the one it needs.
    ///
    /// # Errors
    ///
    /// If the extent table or any extent fails its check.
    pub fn payload(&self, of: &Section) -> Result<Vec<u8>> {
        let extents = self.extents(of)?;
        let mut bytes =
            Vec::with_capacity(sum(extents.iter().map(|one| u64::from(one.length))) as usize);
        for one in &extents {
            if one.first != bytes.len() as u64 {
                return Err(invalid("a section's extents do not join up"));
            }
            bytes.extend_from_slice(&self.extent(one)?);
        }
        // The same exception `write_section` makes: a budget record has no bytes, so its
        // `header_bytes` is a size rather than a header and there is nothing for it to run past.
        if !bytes.is_empty() && of.header_bytes as usize > bytes.len() {
            return Err(invalid("a section's header is longer than its payload"));
        }
        Ok(bytes)
    }

    /// Reads only the named columns from one part.
    ///
    /// The whole stripe page each column lives in is read and kept, because a scan asks for the
    /// parts of a stripe one after another and this is what turns sixty four reads into one.
    ///
    /// # Errors
    ///
    /// If a part, column, page, or checksum is invalid.
    pub fn read(&self, part: usize, columns: &[usize]) -> Result<Chunk> {
        self.read_impl(part, columns, true, None)
    }

    /// Reads named columns from one part without keeping the stripe page it came out of.
    ///
    /// This is for sparse row fetches after a selective TopN or filter, which reach a few parts of
    /// a stripe rather than all of them. A caller that will read most of a stripe should use
    /// [`Self::read`] instead, because this reads and discards the page index every time.
    ///
    /// # Errors
    ///
    /// If a part, column, page, or checksum is invalid.
    pub fn read_sparse(&self, part: usize, columns: &[usize]) -> Result<Chunk> {
        self.read_impl(part, columns, false, None)
    }

    /// Counts one signed integer part from its encoded row values when it uses an all-valid
    /// cascade. Sparse and run-length cascades are folded without expanding their rows. Other
    /// page forms return `None` so the caller can use the ordinary reader.
    ///
    /// # Errors
    ///
    /// If a part, column, page checksum, or encoded integer is invalid.
    pub fn integer_tally(&self, part: usize, column: usize) -> Result<Option<Vec<(i64, u64)>>> {
        let place = *self.places.get(part).ok_or_else(|| invalid("part index out of range"))?;
        let field =
            self.table.fields.get(column).ok_or_else(|| invalid("column index out of range"))?;
        if !matches!(
            field.ty,
            LogicalType::TinyInt
                | LogicalType::SmallInt
                | LogicalType::Integer
                | LogicalType::BigInt
        ) {
            return Ok(None);
        }
        let stripe_index = place.stripe as usize;
        let stripe = self
            .table
            .stripes
            .get(stripe_index)
            .ok_or_else(|| invalid("stripe index out of range"))?;
        let page = stripe.pages.get(column).ok_or_else(|| invalid("stripe page is missing"))?;
        let held = self.held(stripe_index, stripe, column, true)?;
        let span = *held
            .index
            .get(place.part as usize)
            .ok_or_else(|| invalid("part index out of range"))?;
        let owned;
        let bytes = match &held.page {
            Some(page) => page.part(place.part as usize, span)?,
            None => {
                let offset = page
                    .offset
                    .checked_add(span.start as u64)
                    .ok_or_else(|| invalid("part range overflow"))?;
                let mut bytes = vec![0; span.length];
                read_at(&self.file, offset, &mut bytes)?;
                verify_part(&bytes, span)?;
                owned = bytes;
                &owned
            }
        };
        if bytes.first() != Some(&5) || bytes.get(1) != Some(&0) {
            return Ok(None);
        }
        let (rows, counts) = integer::tally(&bytes[2..])?;
        if rows != place.rows as usize {
            return Err(invalid("encoded integer part holds the wrong number of rows"));
        }
        for &(value, _) in &counts {
            let fits = match field.ty {
                LogicalType::TinyInt => i8::try_from(value).is_ok(),
                LogicalType::SmallInt => i16::try_from(value).is_ok(),
                LogicalType::Integer => i32::try_from(value).is_ok(),
                LogicalType::BigInt => true,
                _ => false,
            };
            if !fits {
                return Err(invalid("encoded integer value is outside its column type"));
            }
        }
        Ok(Some(counts))
    }

    /// Reads named columns from one part, only at the rows `positions` names.
    ///
    /// For a scan that already knows which rows of the part it keeps, from the columns it read
    /// first. A compressed string page decompresses only those rows, and every other page is
    /// decoded whole and gathered, which is what reading it and narrowing it costs anyway. With
    /// `whole` the stripe's pages are kept the way [`Self::read`] keeps them, and without it they
    /// are not, the way [`Self::read_sparse`] does.
    ///
    /// # Errors
    ///
    /// If a part, column, page, or checksum is invalid, or the positions do not rise or run past
    /// the end of the part.
    pub fn read_rows(
        &self,
        part: usize,
        columns: &[usize],
        positions: &[u32],
        whole: bool,
    ) -> Result<Chunk> {
        self.read_impl(part, columns, whole, Some(positions))
    }

    /// Whether an exact global-code membership index proves that the stripe holding a part cannot
    /// contain any of the sorted candidate codes.
    ///
    /// # Errors
    ///
    /// If the part, column, index page, checksum, or delta stream is invalid.
    pub fn skips_codes(&self, part: usize, column: usize, candidates: &[u32]) -> Result<bool> {
        // A demoted column's later stripes hold values the dictionary never coded, so no list of
        // codes can prove a stripe of it holds none of a value.
        if self.demoted(column) {
            return Ok(false);
        }
        if candidates.is_empty() {
            return Ok(true);
        }
        if candidates.windows(2).any(|pair| pair[0] >= pair[1]) {
            return Err(Error::internal("native code candidates are not sorted and unique"));
        }
        let stripe = self.stripe_of(part)?;
        let Some(page) = stripe.memberships.get(column) else {
            return Ok(false);
        };
        let mut bytes = vec![0; page.length as usize];
        read_at(&self.file, page.offset, &mut bytes)?;
        if checksum(&bytes) != page.hash {
            return Err(invalid("membership page checksum differs"));
        }
        let codes = decode_membership(&bytes)?;
        let mut left = 0;
        let mut right = 0;
        while left < codes.len() && right < candidates.len() {
            match codes[left].cmp(&candidates[right]) {
                Ordering::Less => left += 1,
                Ordering::Greater => right += 1,
                Ordering::Equal => return Ok(false),
            }
        }
        Ok(true)
    }

    fn stripe_of(&self, part: usize) -> Result<&Stripe> {
        let place = self.places.get(part).ok_or_else(|| invalid("part index out of range"))?;
        self.table
            .stripes
            .get(place.stripe as usize)
            .ok_or_else(|| invalid("stripe index out of range"))
    }

    /// The page index of one column of one stripe, and its page when the caller wants all of it.
    ///
    /// A scan hands parts out in order, so every worker on a column crosses into a new stripe within
    /// a few parts of the others and they all want the same page at the same moment. This used to
    /// let all of them read it, which cost the scan as many copies of every page as it had workers.
    /// On the full ClickBench file a `MIN(EventDate), MAX(EventDate)` moved 3.2 GB off the disk to
    /// look at 400 MB of column.
    ///
    /// A worker that finds the page it wants already being read neither waits for it nor reads it
    /// again. It comes back with the index alone, which sends [`Reader::read_impl`] down the path
    /// that reads the one part it came for, a few kilobytes against a quarter of a megabyte, and it
    /// picks the page up from the cache on its next part. Waiting would be the other way to avoid
    /// the duplicate read and it is worse: the pages that matter are the wide string ones, they take
    /// milliseconds to copy even warm, and every other worker would be stopped for all of it.
    ///
    /// The file is never read under the lock.
    fn held(&self, at: usize, stripe: &Stripe, column: usize, whole: bool) -> Result<CachedColumn> {
        let cache =
            self.cache.columns.get(column).ok_or_else(|| invalid("column index out of range"))?;
        let mut cached = cache.lock().map_err(|_| invalid("column page cache is poisoned"))?;
        let known = cached.index.get(at).and_then(Clone::clone);
        let page = cached.pages.get(at).and_then(Option::as_ref).map(|slot| {
            slot.used.store(true, Atomic::Relaxed);
            Arc::clone(&slot.page)
        });
        if let Some(index) = known.clone() {
            if !whole || page.is_some() {
                return Ok(CachedColumn { stripe: at, index, page });
            }
        }
        if cached.loading.contains(&at) {
            drop(cached);
            // The index is almost always already here, because somebody read this stripe to get
            // into the loading list in the first place, so this branch usually costs no read at
            // all and the one part read in `read_impl` is all the losing worker pays for.
            if let Some(index) = known {
                return Ok(CachedColumn { stripe: at, index, page: None });
            }
            let held = self.page_of(stripe, column, at, false, None)?;
            let mut cached = cache.lock().map_err(|_| invalid("column page cache is poisoned"))?;
            remember(&mut cached, &held);
            return Ok(held);
        }
        cached.loading.push(at);
        drop(cached);

        let read = self.page_of(stripe, column, at, whole, known);

        // The stripe leaves the loading list and its page enters the cache under one lock. Doing
        // them separately would leave a moment where another worker sees neither and reads the
        // page a second time, which is the whole thing this is here to stop.
        let mut cached = cache.lock().map_err(|_| invalid("column page cache is poisoned"))?;
        if let Some(position) = cached.loading.iter().position(|loading| *loading == at) {
            cached.loading.remove(position);
        }
        let held = read?;
        let taken = remember(&mut cached, &held);
        let first = taken.is_some()
            && cached.seen.get_mut(at).is_some_and(|seen| !std::mem::replace(seen, true));
        if first {
            let floor = self.cache.kept.load(Atomic::Relaxed).max(1);
            cached.passing.push_back(at);
            while cached.passing.len() > floor {
                let Some(old) = cached.passing.pop_front() else { break };
                if let Some(slot) = cached.pages.get_mut(old) {
                    *slot = None;
                }
            }
            return Ok(held);
        }
        drop(cached);
        if let Some((bytes, used)) = taken {
            self.cache.held[column].fetch_add(1, Atomic::Relaxed);
            self.pool.admit(Held {
                shelf: Arc::downgrade(&self.cache),
                column,
                stripe: at,
                bytes,
                used,
            });
        }
        Ok(held)
    }

    /// Reads one stripe's index for a column, and its page when the caller wants all of it.
    ///
    /// `known` is the index when the reader has already read it, which after the first worker
    /// through a stripe it always has, because [`remember`] keeps every index for the life of the
    /// reader. Without that a scan reads the index again on every part that misses the page cache.
    fn page_of(
        &self,
        stripe: &Stripe,
        column: usize,
        at: usize,
        whole: bool,
        known: Option<Arc<Vec<PartSpan>>>,
    ) -> Result<CachedColumn> {
        let index = match known {
            Some(index) => index,
            None => {
                self.indexes.fetch_add(1, Atomic::Relaxed);
                Arc::new(read_index(&self.file, stripe, column)?)
            }
        };
        let page = if whole {
            self.pages.fetch_add(1, Atomic::Relaxed);
            let span = stripe.pages.get(column).ok_or_else(|| invalid("stripe page is missing"))?;
            let mut bytes = vec![0; span.length as usize];
            read_at(&self.file, span.offset, &mut bytes)?;
            let checked = index.iter().map(|_| AtomicBool::new(false)).collect();
            Some(Arc::new(HeldPage { bytes, checked }))
        } else {
            None
        };
        Ok(CachedColumn { stripe: at, index, page })
    }

    fn read_impl(
        &self,
        at: usize,
        columns: &[usize],
        whole: bool,
        positions: Option<&[u32]>,
    ) -> Result<Chunk> {
        let place = *self.places.get(at).ok_or_else(|| invalid("part index out of range"))?;
        let index = place.stripe as usize;
        let stripe =
            self.table.stripes.get(index).ok_or_else(|| invalid("stripe index out of range"))?;
        let rows = place.rows as usize;
        let mut picked = Vec::with_capacity(columns.len());
        for &column in columns {
            let field = self
                .table
                .fields
                .get(column)
                .ok_or_else(|| invalid("column index out of range"))?;
            let page = stripe.pages.get(column).ok_or_else(|| invalid("stripe page is missing"))?;
            let held = self.held(index, stripe, column, whole)?;
            let span = *held
                .index
                .get(place.part as usize)
                .ok_or_else(|| invalid("part index out of range"))?;
            let owned;
            let bytes = match &held.page {
                Some(held) => held.part(place.part as usize, span),
                None => {
                    let offset = page
                        .offset
                        .checked_add(span.start as u64)
                        .ok_or_else(|| invalid("part range overflow"))?;
                    let mut bytes = vec![0; span.length];
                    read_at(&self.file, offset, &mut bytes)?;
                    owned = bytes;
                    verify_part(&owned, span).map(|()| owned.as_slice())
                }
            }
            .map_err(|error| {
                invalid(&format!(
                    "{}, column {column} part {} of the page at {}",
                    error.message(),
                    place.part,
                    page.offset,
                ))
            })?;
            let dictionary = self.dictionary(column)?;
            // Held as a page, because a column that came out of a file is handed out more than
            // once. A group by clones its key columns out of the chunk so the keys outlive it, a
            // projection of a bare column name does the same, and a cut of a flat run copies unless
            // the run is a page. One `Arc` per column per part buys all of those, and it moves the
            // run into the `Arc` without touching a value.
            let mut vector = match positions {
                None => decode(&field.ty, rows, bytes, dictionary)?,
                Some(positions) => decode_at(&field.ty, rows, bytes, dictionary, positions)?,
            };
            // A demoted column's codes are not the column's codes, only the codes of the stripes
            // written before the demotion, so they are not handed out as if they were. See
            // [`DEMOTED`].
            if self.demoted(column) && vector.stable_dictionary_parts().is_some() {
                vector = vector.flatten()?;
            }
            picked.push(vector.into_pages());
        }
        Chunk::with_rows(picked, positions.map_or(rows, <[u32]>::len))
    }

    /// Whether persisted statistics prove that a part cannot match the predicates.
    ///
    /// Three of them, asked cheapest first.
    ///
    /// The stripe's bounds are in memory already, so they are free, and they are also the coarsest:
    /// every part of a stripe gets the same answer and a scan that skips one part that way skips all
    /// sixty four. Then the part's own bounds, which are a read of one page per column per stripe
    /// and are sixty four times finer. Then the sieves, which are per part and answer equality, the
    /// test bounds are worst at: a column of identifiers has every stripe and nearly every part
    /// covering the whole of its type, so bounds keep them all and the sieve keeps the ones that
    /// really hold the value.
    ///
    /// The middle one is what an ordered comparison on a column the rows are not sorted by needs. On
    /// ClickBench 24 the stripe bounds leave eight stripes of sixteen alive, which is half the file,
    /// and the part bounds leave thirty parts of nine hundred and seventy four.
    #[must_use]
    pub fn skips(&self, part: usize, probes: &[Probe]) -> bool {
        let Some(place) = self.places.get(part).copied() else { return false };
        let Some(stripe) = self.table.stripes.get(place.stripe as usize) else { return false };
        if stripe.zone.skips(probes) {
            return true;
        }
        probes.iter().any(|probe| self.outside(place, probe) || self.sifted(place, probe))
    }

    /// Whether the bounds of one part rule out one probe.
    ///
    /// The part's own two ends, which are narrower than the stripe's and cost a page read the first
    /// time this is asked about a column. A column with no page here answers `false`, which is the
    /// answer a caller got before there were any.
    fn outside(&self, place: Place, probe: &Probe) -> bool {
        match self.stripe_part_ranges(place.stripe as usize, probe.column) {
            Some(ranges) => ranges
                .get(place.part as usize)
                .is_some_and(|range| range.excludes(probe.op, &probe.value)),
            None => false,
        }
    }

    /// The per part ranges of one stripe of one column, read once and kept.
    ///
    /// `None` when the column has no page in that stripe and when the page is damaged, on the same
    /// reasoning as the sieves: this is an index over data that is still there, so a caller that
    /// cannot read one reads the rows and gets the right answer slowly.
    fn stripe_part_ranges(&self, stripe: usize, column: usize) -> Option<&[Range]> {
        let slot = self.part_ranges.get(column)?.get(stripe)?;
        if let Some(held) = slot.get() {
            return Some(held);
        }
        let page = self.table.stripes.get(stripe)?.part_ranges.get(column)?;
        let mut bytes = vec![0; page.length as usize];
        read_at(&self.file, page.offset, &mut bytes).ok()?;
        if checksum(&bytes) != page.hash {
            return None;
        }
        let ranges = Arc::new(decode_part_ranges(&bytes).ok()?);
        let _ = slot.set(ranges);
        slot.get().map(|held| held.as_slice())
    }

    /// Whether persisted statistics prove that every row of a part matches the predicates.
    ///
    /// Only the bounds. The sieves say nothing here, because a sieve that holds a value is a sieve
    /// that may be holding somebody else's hash, so it can rule a part out and can never wave one
    /// through.
    ///
    /// The stripe first and the part after it, the same two steps and in the same order as
    /// [`Self::skips`]. The stripe's bounds are in memory already and its null count covers sixty
    /// four parts rather than one, so a stripe that answers is an answer for nothing, and the part's
    /// own bounds are only read for the probes it could not settle. Both directions are safe: a
    /// stretch where everything passes contains no narrower stretch where something fails, and a
    /// stripe with no nulls has no nulls in any of its parts.
    ///
    /// A string end a part recorded is cut down to its first few bytes, so a part's stretch can be
    /// wider than its rows really are as well. That is the same safe direction for the same reason,
    /// and it is why this asks the two ends rather than anything `exact` says.
    #[must_use]
    pub fn certain(&self, part: usize, probes: &[Probe]) -> bool {
        let Some(place) = self.places.get(part).copied() else { return false };
        let Some(stripe) = self.table.stripes.get(place.stripe as usize) else { return false };
        if stripe.zone.certain(probes) {
            return true;
        }
        probes
            .iter()
            .all(|probe| stripe.zone.certain(slice::from_ref(probe)) || self.inside(place, probe))
    }

    /// Whether one part's own two ends prove that every row of it passes `probe`.
    ///
    /// The mirror of [`Self::outside`], reading the same page. `false` for a part whose stripe wrote
    /// no range page, which is a stripe of one part, because there the stripe's own bounds are the
    /// part's and the caller has already asked them.
    fn inside(&self, place: Place, probe: &Probe) -> bool {
        match self.stripe_part_ranges(place.stripe as usize, probe.column) {
            Some(ranges) => ranges
                .get(place.part as usize)
                .is_some_and(|range| range.certain(probe.op, &probe.value)),
            None => false,
        }
    }

    /// Whether the bounds of one stripe prove that none of its parts can match the predicates.
    ///
    /// The cheap half of [`Self::skips`], asked about a whole stripe at once. The bounds live in the
    /// directory and are already in memory, so this answers without touching the file, and that is
    /// the reason it is worth having on its own: a caller that wants to know roughly where the work
    /// is before it starts any workers can ask this about sixteen stripes for nothing, where asking
    /// [`Self::skips`] about nine hundred parts would read and decode a sieve page per stripe first.
    ///
    /// It keeps stripes that [`Self::skips`] would rule out part by part, which is the right way for
    /// it to be wrong: the parts are still checked when they are read.
    #[must_use]
    pub fn stripe_skips(&self, stripe: usize, probes: &[Probe]) -> bool {
        self.table.stripes.get(stripe).is_some_and(|held| held.zone.skips(probes))
    }

    /// Whether the sieve of one part rules out one probe.
    ///
    /// Only equality. An ordered comparison is what the bounds are for and a sieve says nothing
    /// about it, and a read that cannot answer keeps the part, which is the answer a caller with no
    /// sieve gets anyway.
    fn sifted(&self, place: Place, probe: &Probe) -> bool {
        if probe.op != Op::Equal {
            return false;
        }
        match self.stripe_sieves(place.stripe as usize, probe.column) {
            Some(sieves) => sieves
                .get(place.part as usize)
                .and_then(Option::as_ref)
                .is_some_and(|sieve| sieve.excludes(&probe.value)),
            None => false,
        }
    }

    /// The sieves of one stripe of one column, read once and kept.
    ///
    /// `None` when the column has no sieves in that stripe, when the page is damaged, and when the
    /// bytes are not a page this version can read. A sieve is an index over data that is still there
    /// and a caller that cannot read one reads the rows, so this is the one place in the file where
    /// a bad checksum is a slow query rather than an error.
    fn stripe_sieves(&self, stripe: usize, column: usize) -> Option<&[Option<Sieve>]> {
        let slot = self.sieves.get(column)?.get(stripe)?;
        if let Some(held) = slot.get() {
            return Some(held);
        }
        let page = self.table.stripes.get(stripe)?.sieves.get(column)?;
        let mut bytes = vec![0; page.length as usize];
        read_at(&self.file, page.offset, &mut bytes).ok()?;
        if checksum(&bytes) != page.hash {
            return None;
        }
        let sieves = Arc::new(decode_sieves(&bytes).ok()?);
        let _ = slot.set(sieves);
        slot.get().map(|held| held.as_slice())
    }
}

/// The value sitting at one position of a dictionary's sorted order.
fn text_at_rank(dictionary: &Vector, rank: usize) -> Result<Value> {
    let code = dictionary.code_at_rank(rank)? as usize;
    if dictionary.logical_type() == &LogicalType::Blob {
        let bytes = dictionary
            .try_bytes_at(code)?
            .ok_or_else(|| invalid("global dictionary order names a code it does not have"))?;
        return Ok(Value::Blob(bytes.to_vec()));
    }
    let text = dictionary
        .try_text_at(code)?
        .ok_or_else(|| invalid("global dictionary order names a code it does not have"))?;
    Ok(Value::Varchar(text.into()))
}

/// Reads one span of a file at an offset, without moving a cursor anybody else can see.
///
/// Every reader of a table shares one [`File`] behind an [`Arc`], and a grouped aggregate reads its
/// pages from several threads at once, so this has to be positional. Seeking and then reading is
/// two calls with a gap in the middle, and in that gap another thread's seek lands and the read
/// comes back with somebody else's bytes.
///
/// The writer reads back through here too, out of the `rudb_io` file it writes through, which is
/// why this takes anything [`Positional`] rather than a [`File`].
fn read_at<F: Positional + ?Sized>(file: &F, offset: u64, bytes: &mut [u8]) -> Result<()> {
    file.fill_at(offset, bytes)
}

/// Something a span of bytes can be read out of by offset.
///
/// There are two of these. The reader holds a `std::fs::File`, because it shares it between its
/// threads behind an [`Arc`] and every read it makes is on the hot path of a scan. The writer holds
/// an `rudb_io::File`, because everything it does to the file has to be something the simulated
/// filesystem can stop and crash. The few helpers both of them use, [`read_index`] and the choice
/// of committed slot, are written once over this rather than once for each.
trait Positional {
    /// Fills `bytes` from `offset`, or fails if the file ends first.
    ///
    /// Both kinds can come back short, so both loop. A read of zero bytes before the span is filled
    /// means the file stops earlier than the directory said it does.
    fn fill_at(&self, offset: u64, bytes: &mut [u8]) -> Result<()>;
}

impl<T: Positional + ?Sized> Positional for &T {
    fn fill_at(&self, offset: u64, bytes: &mut [u8]) -> Result<()> {
        (**self).fill_at(offset, bytes)
    }
}

impl<T: Positional + ?Sized> Positional for Arc<T> {
    fn fill_at(&self, offset: u64, bytes: &mut [u8]) -> Result<()> {
        (**self).fill_at(offset, bytes)
    }
}

impl<T: Positional + ?Sized> Positional for Box<T> {
    fn fill_at(&self, offset: u64, bytes: &mut [u8]) -> Result<()> {
        (**self).fill_at(offset, bytes)
    }
}

impl Positional for dyn rudb_io::File + '_ {
    fn fill_at(&self, mut offset: u64, mut bytes: &mut [u8]) -> Result<()> {
        while !bytes.is_empty() {
            let read = self.read_at(offset, bytes)?;
            if read == 0 {
                return Err(invalid("column page ends before its declared length"));
            }
            offset += read as u64;
            bytes = &mut bytes[read..];
        }
        Ok(())
    }
}

impl Positional for File {
    #[cfg(unix)]
    fn fill_at(&self, mut offset: u64, mut bytes: &mut [u8]) -> Result<()> {
        use std::os::unix::fs::FileExt;
        while !bytes.is_empty() {
            let read = self.read_at(bytes, offset).map_err(io)?;
            if read == 0 {
                return Err(invalid("column page ends before its declared length"));
            }
            offset += read as u64;
            bytes = &mut bytes[read..];
        }
        Ok(())
    }

    /// The same read, on the call Windows spells differently.
    ///
    /// `seek_read` is one `ReadFile` carrying the offset with it, so two of them cannot interleave
    /// the way a seek and a read can. It does leave the shared cursor somewhere afterwards, which is
    /// why nothing in this file may read that cursor.
    #[cfg(windows)]
    fn fill_at(&self, mut offset: u64, mut bytes: &mut [u8]) -> Result<()> {
        use std::os::windows::fs::FileExt;
        while !bytes.is_empty() {
            let read = self.seek_read(bytes, offset).map_err(io)?;
            if read == 0 {
                return Err(invalid("column page ends before its declared length"));
            }
            offset += read as u64;
            bytes = &mut bytes[read..];
        }
        Ok(())
    }

    /// Somewhere that is neither, where the cursor is all there is.
    ///
    /// This one does race, and there is no way to write it so it does not. Nothing we build for
    /// runs here, so it exists to keep the crate compiling rather than to be correct under threads.
    #[cfg(not(any(unix, windows)))]
    fn fill_at(&self, offset: u64, bytes: &mut [u8]) -> Result<()> {
        use std::io::{Read, Seek, SeekFrom};
        let mut file = self.try_clone().map_err(io)?;
        file.seek(SeekFrom::Start(offset)).map_err(io)?;
        file.read_exact(bytes).map_err(io)
    }
}

/// Overwrites one span of a file in place, which is how the tests damage a file on purpose.
///
/// The writer does not come through here. It writes through `rudb_io`, and this is a
/// `std::fs::File` opened by a test beside it.
#[cfg(test)]
fn write_at(file: &File, offset: u64, bytes: &[u8]) -> Result<()> {
    use std::io::{Seek, SeekFrom, Write};
    let mut file = file;
    file.seek(SeekFrom::Start(offset)).map_err(io)?;
    file.write_all(bytes).map_err(io)
}

/// What a column type is called in the directory.
///
/// A tag is a number in a file somebody else wrote, so a tag that has been used is used forever and
/// the only thing that may happen to this list is that it grows. 1 to 13 are the tags the format
/// had when it could store thirteen types, and 14 to 27 are the rest, in the order they were added
/// rather than in an order that means anything.
fn type_tag(ty: &LogicalType) -> Result<u8> {
    match ty {
        LogicalType::SmallInt => Ok(1),
        LogicalType::Integer => Ok(2),
        LogicalType::BigInt => Ok(3),
        LogicalType::Varchar => Ok(4),
        LogicalType::Date => Ok(5),
        LogicalType::Timestamp => Ok(6),
        LogicalType::Boolean => Ok(7),
        LogicalType::TinyInt => Ok(8),
        LogicalType::UTinyInt => Ok(9),
        LogicalType::USmallInt => Ok(10),
        LogicalType::UInteger => Ok(11),
        LogicalType::UBigInt => Ok(12),
        LogicalType::Decimal { .. } => Ok(13),
        LogicalType::Float => Ok(14),
        LogicalType::Double => Ok(15),
        LogicalType::HugeInt => Ok(16),
        LogicalType::UHugeInt => Ok(17),
        LogicalType::Time => Ok(18),
        LogicalType::TimeTz => Ok(19),
        LogicalType::TimestampTz => Ok(20),
        LogicalType::Interval => Ok(21),
        LogicalType::Uuid => Ok(22),
        LogicalType::Blob => Ok(23),
        LogicalType::Bit => Ok(24),
        LogicalType::TimestampS => Ok(25),
        LogicalType::TimestampMs => Ok(26),
        LogicalType::TimestampNs => Ok(27),
        _ => Err(Error::not_implemented(format!("native storage for {ty}"))),
    }
}

/// The tag of a column type, and the parameters of the ones that have any.
///
/// Only `DECIMAL` has parameters today. Width and scale go after the tag rather than into it
/// because they are what says how wide a value is on disk, and a reader that guessed would read the
/// wrong number of bytes per row rather than the wrong number of digits.
fn put_type(out: &mut Vec<u8>, ty: &LogicalType) -> Result<()> {
    out.push(type_tag(ty)?);
    if let LogicalType::Decimal { width, scale } = ty {
        out.push(*width);
        out.push(*scale);
    }
    Ok(())
}

/// The other half of [`put_type`], reading the parameters the tag says are there.
fn read_type(cur: &mut Cursor<'_>) -> Result<LogicalType> {
    let tag = cur.u8()?;
    if tag == 13 {
        let width = cur.u8()?;
        let scale = cur.u8()?;
        return LogicalType::decimal(width, scale)
            .map_err(|_| invalid("decimal column width and scale are not a decimal"));
    }
    tag_type(tag)
}

fn tag_type(tag: u8) -> Result<LogicalType> {
    match tag {
        1 => Ok(LogicalType::SmallInt),
        2 => Ok(LogicalType::Integer),
        3 => Ok(LogicalType::BigInt),
        4 => Ok(LogicalType::Varchar),
        5 => Ok(LogicalType::Date),
        6 => Ok(LogicalType::Timestamp),
        7 => Ok(LogicalType::Boolean),
        8 => Ok(LogicalType::TinyInt),
        9 => Ok(LogicalType::UTinyInt),
        10 => Ok(LogicalType::USmallInt),
        11 => Ok(LogicalType::UInteger),
        12 => Ok(LogicalType::UBigInt),
        14 => Ok(LogicalType::Float),
        15 => Ok(LogicalType::Double),
        16 => Ok(LogicalType::HugeInt),
        17 => Ok(LogicalType::UHugeInt),
        18 => Ok(LogicalType::Time),
        19 => Ok(LogicalType::TimeTz),
        20 => Ok(LogicalType::TimestampTz),
        21 => Ok(LogicalType::Interval),
        22 => Ok(LogicalType::Uuid),
        23 => Ok(LogicalType::Blob),
        24 => Ok(LogicalType::Bit),
        25 => Ok(LogicalType::TimestampS),
        26 => Ok(LogicalType::TimestampMs),
        27 => Ok(LogicalType::TimestampNs),
        _ => Err(invalid("column type tag is unknown")),
    }
}

fn put_u16(out: &mut Vec<u8>, value: u16) {
    out.extend_from_slice(&value.to_le_bytes());
}
fn put_u32(out: &mut Vec<u8>, value: u32) {
    out.extend_from_slice(&value.to_le_bytes());
}
fn put_u64(out: &mut Vec<u8>, value: u64) {
    out.extend_from_slice(&value.to_le_bytes());
}
fn put_var_u64(out: &mut Vec<u8>, mut value: u64) {
    while value >= 0x80 {
        out.push((value as u8 & 0x7f) | 0x80);
        value >>= 7;
    }
    out.push(value as u8);
}

fn frequency_order(left: FrequencyValue, right: FrequencyValue) -> Ordering {
    match (left, right) {
        (FrequencyValue::Null, FrequencyValue::Null) => Ordering::Equal,
        (FrequencyValue::Null, _) => Ordering::Less,
        (_, FrequencyValue::Null) => Ordering::Greater,
        (FrequencyValue::Integer(left), FrequencyValue::Integer(right)) => left.cmp(&right),
        (FrequencyValue::Code(left), FrequencyValue::Code(right)) => left.cmp(&right),
        (FrequencyValue::Integer(_), FrequencyValue::Code(_)) => Ordering::Less,
        (FrequencyValue::Code(_), FrequencyValue::Integer(_)) => Ordering::Greater,
    }
}

/// Leaves the [`FREQUENCY_ENTRIES`] commonest entries in order and says what the next one counted.
///
/// There is one entry a distinct value, so on `URL` this is handed two and a quarter million of
/// them and keeps five hundred and twelve. Sorting all of them to throw almost all of them away is
/// the whole of what counting a dictionary column used to cost, 2.13 seconds of it on `URL` at eight
/// million rows against 11.93 for compressing the same column's values.
///
/// Partitioning answers both questions instead. It puts the five hundred and thirteenth entry where
/// it belongs and everything commoner in front of it, which is the entries to keep and the count to
/// report as the largest one omitted, and then only the part that survives is sorted. The order that
/// comes out is the order the sort gave, because the tie break makes the comparison total: two
/// entries never hold the same value.
fn keep_most_frequent(entries: &mut Vec<FrequencyEntry>) -> u64 {
    let order = |left: &FrequencyEntry, right: &FrequencyEntry| {
        right.count.cmp(&left.count).then_with(|| frequency_order(left.value, right.value))
    };
    let omitted_max = if entries.len() > FREQUENCY_ENTRIES {
        let (_, next, _) = entries.select_nth_unstable_by(FREQUENCY_ENTRIES, order);
        let omitted_max = next.count;
        entries.truncate(FREQUENCY_ENTRIES);
        omitted_max
    } else {
        0
    };
    entries.sort_unstable_by(order);
    omitted_max
}

fn code_frequency(
    dictionary: &GlobalDictionary,
    flat: &[u8],
    bases: &[u64],
) -> Result<(FrequencySummary, Vec<Option<Vec<u8>>>)> {
    let mut entries = dictionary
        .counts
        .iter()
        .enumerate()
        .filter(|(_, count)| **count != 0)
        .map(|(code, &count)| FrequencyEntry { value: FrequencyValue::Code(code as u32), count })
        .collect::<Vec<_>>();
    if dictionary.nulls != 0 {
        entries.push(FrequencyEntry { value: FrequencyValue::Null, count: dictionary.nulls });
    }
    let omitted_max = keep_most_frequent(&mut entries);
    let mut spans = Vec::with_capacity(entries.len());
    let mut text_bytes = 0_usize;
    for entry in &entries {
        let span = match entry.value {
            FrequencyValue::Code(code) => {
                let span = GlobalDictionary::value_span(&dictionary.ends, bases, code as usize);
                let bytes = flat
                    .get(span.0..span.1)
                    .ok_or_else(|| invalid("a frequency code is outside its dictionary"))?;
                text_bytes = text_bytes.saturating_add(bytes.len());
                Some(span)
            }
            FrequencyValue::Null | FrequencyValue::Integer(_) => None,
        };
        spans.push(span);
    }
    let texts = if text_bytes > FREQUENCY_TEXT_BUDGET {
        Vec::new()
    } else {
        spans.into_iter().map(|span| span.map(|(from, to)| flat[from..to].to_vec())).collect()
    };
    Ok((
        FrequencySummary {
            entries,
            omitted_max,
            ordinals: Vec::new(),
            ordinal_entries: Vec::new(),
        },
        texts,
    ))
}

fn encode_directory(table: &Table) -> Result<Vec<u8>> {
    let mut out = DIRECTORY.to_vec();
    let name = table.name.as_bytes();
    put_u16(&mut out, u16::try_from(name.len()).map_err(|_| invalid("table name too long"))?);
    out.extend_from_slice(name);
    put_u16(&mut out, u16::try_from(table.fields.len()).map_err(|_| invalid("too many columns"))?);
    for field in &table.fields {
        let name = field.name.as_bytes();
        put_u16(&mut out, u16::try_from(name.len()).map_err(|_| invalid("column name too long"))?);
        out.extend_from_slice(name);
        put_type(&mut out, &field.ty)?;
        out.push(u8::from(field.not_null));
    }
    for (field, dictionary) in table.fields.iter().zip(&table.dictionaries) {
        match dictionary {
            None => out.push(0),
            Some(page) => {
                out.push(dictionary_tag(&field.ty));
                put_u64(&mut out, page.offset);
                put_u32(&mut out, page.length);
                put_u64(&mut out, page.hash);
            }
        }
    }
    for distinct in &table.distincts {
        match distinct {
            None => out.push(0),
            Some(count) => {
                out.push(1);
                put_u64(&mut out, *count);
            }
        }
    }
    put_u64(&mut out, u64::try_from(table.rows).map_err(|_| invalid("row count overflow"))?);
    put_u32(&mut out, u32::try_from(table.stripes.len()).map_err(|_| invalid("too many stripes"))?);
    for stripe in &table.stripes {
        put_u32(
            &mut out,
            u32::try_from(stripe.parts.len()).map_err(|_| invalid("too many parts in a stripe"))?,
        );
        for &rows in &stripe.parts {
            put_u32(&mut out, rows);
        }
        put_u64(&mut out, stripe.index.offset);
        put_u32(&mut out, stripe.index.length);
        for page in &stripe.pages {
            put_u64(&mut out, page.offset);
            put_u32(&mut out, page.length);
        }
        // A membership index says which of a dictionary's codes a part holds, so a column the writer
        // decided against giving a dictionary has nothing for it to be about and writes none. Every
        // file written before that decision existed has a dictionary on every varchar column, so
        // this reads those files byte for byte the way it always did.
        for (column, ((field, dictionary), membership)) in
            table.fields.iter().zip(&table.dictionaries).zip(stripe.memberships.slots()).enumerate()
        {
            if !coded_type(&field.ty) || dictionary.is_none() {
                continue;
            }
            let page = match membership {
                Some(page) => page,
                None if table.demoted.get(column).copied().unwrap_or(false) => {
                    Page { offset: HEADER, length: 0, hash: 0 }
                }
                None => return Err(invalid("string page has no code membership index")),
            };
            put_u64(&mut out, page.offset);
            put_u32(&mut out, page.length);
            put_u64(&mut out, page.hash);
        }
        for sieve in stripe.sieves.slots() {
            match sieve {
                None => out.push(0),
                Some(page) => {
                    out.push(1);
                    put_u64(&mut out, page.offset);
                    put_u32(&mut out, page.length);
                    put_u64(&mut out, page.hash);
                }
            }
        }
        for held in stripe.part_ranges.slots() {
            match held {
                None => out.push(0),
                Some(page) => {
                    out.push(1);
                    put_u64(&mut out, page.offset);
                    put_u32(&mut out, page.length);
                    put_u64(&mut out, page.hash);
                }
            }
        }
        for range in stripe.zone.columns() {
            put_bound(&mut out, range.low.as_ref())?;
            put_bound(&mut out, range.high.as_ref())?;
            put_u32(
                &mut out,
                u32::try_from(range.nulls).map_err(|_| invalid("null count overflow"))?,
            );
            out.push(u8::from(range.exact));
            match range.sum {
                None => out.push(0),
                Some(total) => {
                    out.push(1);
                    out.extend_from_slice(&total.to_le_bytes());
                }
            }
        }
    }
    out.extend_from_slice(FREQUENCIES);
    put_u16(
        &mut out,
        u16::try_from(table.frequencies.len())
            .map_err(|_| invalid("too many frequency columns"))?,
    );
    for summary in &table.frequencies {
        let summary = match summary {
            None => {
                out.push(0);
                continue;
            }
            Some(Frequencies::Held(summary)) => summary,
            // Only a reader leaves a synopsis in the file, and nothing writes a reader's table back.
            Some(Frequencies::Stored { .. }) => {
                return Err(invalid("a synopsis left in the file cannot be written back"));
            }
        };
        out.push(1);
        put_u64(&mut out, summary.omitted_max);
        put_u32(
            &mut out,
            u32::try_from(summary.entries.len())
                .map_err(|_| invalid("too many frequency entries"))?,
        );
        for entry in &summary.entries {
            match entry.value {
                FrequencyValue::Null => out.push(0),
                FrequencyValue::Integer(value) => {
                    out.push(1);
                    out.extend_from_slice(&value.to_le_bytes());
                }
                FrequencyValue::Code(value) => {
                    out.push(2);
                    put_u32(&mut out, value);
                }
            }
            put_u64(&mut out, entry.count);
        }
        put_u32(
            &mut out,
            u32::try_from(summary.ordinals.len())
                .map_err(|_| invalid("too many frequency ordinals"))?,
        );
        let mut previous = 0_u64;
        for (at, &ordinal) in summary.ordinals.iter().enumerate() {
            let delta = if at == 0 {
                ordinal
            } else {
                ordinal
                    .checked_sub(previous)
                    .ok_or_else(|| invalid("frequency ordinals are not ordered"))?
            };
            if at != 0 && delta == 0 {
                return Err(invalid("frequency ordinals are not unique"));
            }
            put_var_u64(&mut out, delta);
            previous = ordinal;
        }
        if summary.ordinal_entries.len() != summary.ordinals.len() {
            return Err(invalid("frequency ordinal values have a different length"));
        }
        for &entry in &summary.ordinal_entries {
            if entry as usize >= summary.entries.len() {
                return Err(invalid("frequency ordinal value is outside its entries"));
            }
            put_u16(&mut out, entry);
        }
    }
    if !table.pair_frequencies.is_empty() {
        out.extend_from_slice(PAIR_FREQUENCIES);
        put_u16(
            &mut out,
            u16::try_from(table.pair_frequencies.len())
                .map_err(|_| invalid("too many pair frequency summaries"))?,
        );
        for summary in &table.pair_frequencies {
            put_u16(&mut out, summary.first);
            put_u16(&mut out, summary.second);
            put_u64(&mut out, summary.omitted_max);
            put_u16(
                &mut out,
                u16::try_from(summary.entries.len())
                    .map_err(|_| invalid("too many pair frequency entries"))?,
            );
            for entry in &summary.entries {
                put_u16(&mut out, entry.first_entry);
                match entry.second {
                    None => out.push(0),
                    Some(code) => {
                        out.push(1);
                        put_u32(&mut out, code);
                    }
                }
                put_u64(&mut out, entry.count);
            }
        }
    }
    let text_columns = table.frequency_texts.iter().filter(|texts| !texts.is_empty()).count();
    if text_columns != 0 {
        out.extend_from_slice(FREQUENCY_TEXTS);
        put_u16(
            &mut out,
            u16::try_from(text_columns)
                .map_err(|_| invalid("too many string frequency columns"))?,
        );
        for (column, texts) in table.frequency_texts.iter().enumerate() {
            if texts.is_empty() {
                continue;
            }
            put_u16(
                &mut out,
                u16::try_from(column).map_err(|_| invalid("frequency text column overflows"))?,
            );
            put_u16(
                &mut out,
                u16::try_from(texts.len())
                    .map_err(|_| invalid("too many frequency text entries"))?,
            );
            for text in texts {
                match text {
                    None => out.push(0),
                    Some(text) => {
                        out.push(1);
                        put_u32(
                            &mut out,
                            u32::try_from(text.len())
                                .map_err(|_| invalid("frequency text is too long"))?,
                        );
                        out.extend_from_slice(text);
                    }
                }
            }
        }
    }
    if let Some(summary) = &table.host_groups {
        out.extend_from_slice(HOST_GROUPS);
        put_u16(
            &mut out,
            u16::try_from(summary.column).map_err(|_| invalid("host column overflows"))?,
        );
        put_u64(&mut out, summary.omitted_max);
        put_u16(
            &mut out,
            u16::try_from(summary.entries.len()).map_err(|_| invalid("too many host groups"))?,
        );
        for entry in &summary.entries {
            put_u32(
                &mut out,
                u32::try_from(entry.host.len()).map_err(|_| invalid("host name is too long"))?,
            );
            out.extend_from_slice(entry.host.as_bytes());
            put_u64(&mut out, entry.count);
            out.extend_from_slice(&entry.bytes_sum.to_le_bytes());
            put_u32(
                &mut out,
                u32::try_from(entry.minimum.len())
                    .map_err(|_| invalid("host minimum is too long"))?,
            );
            out.extend_from_slice(entry.minimum.as_bytes());
        }
    }
    // Written only when there is a declaration, so that the common file is the same bytes it was
    // and the section is not a byte of zero on every table in the world that never asked for one.
    if let Some(clustering) = &table.clustering {
        out.extend_from_slice(CLUSTERING);
        out.push(clustering.width().tag());
        put_u16(
            &mut out,
            u16::try_from(clustering.columns().len())
                .map_err(|_| invalid("too many clustering columns"))?,
        );
        for &column in clustering.columns() {
            put_u16(
                &mut out,
                u16::try_from(column).map_err(|_| invalid("clustering column index overflow"))?,
            );
        }
    }
    let demoted = (0..table.fields.len())
        .filter(|&column| table.demoted.get(column).copied().unwrap_or(false))
        .collect::<Vec<_>>();
    if !demoted.is_empty() {
        out.extend_from_slice(DEMOTED);
        put_u16(
            &mut out,
            u16::try_from(demoted.len()).map_err(|_| invalid("too many demoted columns"))?,
        );
        for column in demoted {
            put_u16(
                &mut out,
                u16::try_from(column).map_err(|_| invalid("demoted column index overflow"))?,
            );
        }
    }
    // The section table, last, behind its own magic, for the same reason the frequency block is
    // behind its own: a reader that stops before it gets a table with no sections, and a table with
    // no sections is a correct table. The one difference from the blocks before it is that this one
    // is written even when it is empty, so that a file written by this build always says which
    // sections it has rather than leaving a reader to infer it from where the bytes ran out.
    out.extend_from_slice(SECTIONS);
    put_u64(&mut out, table.generation);
    put_u16(
        &mut out,
        u16::try_from(table.sections.len()).map_err(|_| invalid("too many sections"))?,
    );
    for held in &table.sections {
        held.encode(&mut out)?;
    }
    if table.dictionary_payloads.iter().any(|&bytes| bytes != 0) {
        out.extend_from_slice(DICTIONARY_PAYLOADS);
        put_u16(
            &mut out,
            u16::try_from(table.fields.len()).map_err(|_| invalid("too many columns"))?,
        );
        for at in 0..table.fields.len() {
            put_u64(&mut out, table.dictionary_payloads.get(at).copied().unwrap_or(0));
        }
    }
    Ok(out)
}

/// The small level of the directory, naming every table in the file.
///
/// This is what a footer slot points at. Each entry carries its own checksum over its table
/// directory, so a table whose directory is torn is found when that table is first touched rather
/// than being trusted because the catalog around it checksummed.
///
/// The views go after the tables and are whole here, since a view is text and a column list and has
/// no pages for a second level to point at.
fn signed_integer(ty: &LogicalType) -> bool {
    matches!(
        ty,
        LogicalType::TinyInt | LogicalType::SmallInt | LogicalType::Integer | LogicalType::BigInt
    )
}

fn integer_or_date(ty: &LogicalType) -> bool {
    matches!(
        ty,
        LogicalType::TinyInt
            | LogicalType::SmallInt
            | LogicalType::Integer
            | LogicalType::BigInt
            | LogicalType::UTinyInt
            | LogicalType::USmallInt
            | LogicalType::UInteger
            | LogicalType::UBigInt
            | LogicalType::Date
    )
}

fn table_integer_extremes(table: &Table) -> Vec<StoredIntegerExtremes> {
    table
        .fields
        .iter()
        .enumerate()
        .map(|(column, field)| {
            if !integer_or_date(&field.ty) {
                return None;
            }
            let mut low: Option<i128> = None;
            let mut high: Option<i128> = None;
            for stripe in &table.stripes {
                let range = stripe.zone.column(column)?;
                if !range.exact {
                    return None;
                }
                match (range.low.as_ref(), range.high.as_ref()) {
                    (Some(Bound::Int(small)), Some(Bound::Int(large))) => {
                        low = Some(low.map_or(*small, |held| held.min(*small)));
                        high = Some(high.map_or(*large, |held| held.max(*large)));
                    }
                    (None, None) if stripe.rows == range.nulls => {}
                    _ => return None,
                }
            }
            Some(low.zip(high))
        })
        .collect()
}

fn reader_integer_extremes(reader: &Reader) -> Result<Vec<StoredIntegerExtremes>> {
    reader
        .table
        .fields
        .iter()
        .enumerate()
        .map(|(column, field)| {
            if !integer_or_date(&field.ty) {
                return Ok(None);
            }
            match reader.exact_extremes(column)? {
                Some((Bound::Int(low), Bound::Int(high))) => Ok(Some(Some((low, high)))),
                None if reader.null_count(column)? == reader.table.rows as u64 => Ok(Some(None)),
                _ => Ok(None),
            }
        })
        .collect()
}

fn table_complete_numeric_frequencies(table: &Table) -> Vec<StoredNumericFrequencies> {
    table
        .fields
        .iter()
        .enumerate()
        .map(|(column, field)| {
            if !integer_or_date(&field.ty) {
                return None;
            }
            let Some(Frequencies::Held(summary)) = table.frequencies.get(column)?.as_ref() else {
                return None;
            };
            if summary.omitted_max != 0 || summary.entries.len() > MAX_CATALOG_FREQUENCIES {
                return None;
            }
            let entries = summary
                .entries
                .iter()
                .map(|entry| {
                    let value = match entry.value {
                        FrequencyValue::Null => None,
                        FrequencyValue::Integer(value) => Some(value),
                        FrequencyValue::Code(_) => return None,
                    };
                    Some((value, entry.count))
                })
                .collect::<Option<Vec<_>>>()?;
            let rows = entries.iter().try_fold(0_u64, |sum, (_, count)| sum.checked_add(*count))?;
            (rows == table.rows as u64).then_some(entries)
        })
        .collect()
}

/// The sixty four bits the close keys a numeric column's frequencies by, for a value the writer's
/// tally held.
///
/// The same bits [`Writer::visit_numeric`] hands over: a signed value sign extended to `i64`, and an
/// unsigned one as it is.
/// The value a column's sixty four bits stand for, read as signed or unsigned the way the column is.
fn integer_value(bits: u64, signed: bool) -> FrequencyValue {
    if signed {
        FrequencyValue::Integer(i128::from(bits as i64))
    } else {
        FrequencyValue::Integer(i128::from(bits))
    }
}

fn frequency_bits(value: &Value) -> Option<u64> {
    Some(match value {
        Value::TinyInt(value) => i64::from(*value) as u64,
        Value::SmallInt(value) => i64::from(*value) as u64,
        Value::Integer(value) | Value::Date(value) => i64::from(*value) as u64,
        Value::BigInt(value) | Value::Timestamp(value) => *value as u64,
        Value::UTinyInt(value) => u64::from(*value),
        Value::USmallInt(value) => u64::from(*value),
        Value::UInteger(value) => u64::from(*value),
        Value::UBigInt(value) => *value,
        _ => return None,
    })
}

fn numeric_frequency_value(value: &Value) -> Option<Option<i128>> {
    Some(match value {
        Value::Null => None,
        Value::TinyInt(value) => Some(i128::from(*value)),
        Value::SmallInt(value) => Some(i128::from(*value)),
        Value::Integer(value) | Value::Date(value) => Some(i128::from(*value)),
        Value::BigInt(value) => Some(i128::from(*value)),
        Value::UTinyInt(value) => Some(i128::from(*value)),
        Value::USmallInt(value) => Some(i128::from(*value)),
        Value::UInteger(value) => Some(i128::from(*value)),
        Value::UBigInt(value) => Some(i128::from(*value)),
        _ => return None,
    })
}

fn reader_complete_numeric_frequencies(reader: &Reader) -> Result<Vec<StoredNumericFrequencies>> {
    reader
        .table
        .fields
        .iter()
        .enumerate()
        .map(|(column, field)| {
            if !integer_or_date(&field.ty) {
                return Ok(None);
            }
            let Some(summary) = reader.frequency_summary(column)? else { return Ok(None) };
            if summary.omitted_max != 0 || summary.entries.len() > MAX_CATALOG_FREQUENCIES {
                return Ok(None);
            }
            let entries = reader.decode_frequencies(column, &field.ty, &summary.entries)?;
            let Some(entries) = entries
                .iter()
                .map(|(value, count)| Some((numeric_frequency_value(value)?, *count)))
                .collect::<Option<Vec<_>>>()
            else {
                return Ok(None);
            };
            let rows = entries.iter().try_fold(0_u64, |sum, (_, count)| sum.checked_add(*count));
            Ok((rows == Some(reader.table.rows as u64)).then_some(entries))
        })
        .collect()
}

fn table_exact_sum(table: &Table, column: usize) -> Option<(i128, u64)> {
    table.stripes.iter().try_fold((0_i128, 0_u64), |(sum, count), stripe| {
        let range = stripe.zone.column(column)?;
        let sum = sum.checked_add(range.sum?)?;
        let nonnull = (stripe.rows as u64).checked_sub(range.nulls as u64)?;
        Some((sum, count.checked_add(nonnull)?))
    })
}

fn table_aggregate_sums(table: &Table) -> Vec<Option<(i128, u64)>> {
    table
        .fields
        .iter()
        .enumerate()
        .map(|(column, field)| {
            signed_integer(&field.ty).then(|| table_exact_sum(table, column)).flatten()
        })
        .collect()
}

fn reader_aggregate_sums(reader: &Reader) -> Result<Vec<Option<(i128, u64)>>> {
    reader
        .table
        .fields
        .iter()
        .enumerate()
        .map(
            |(column, field)| {
                if signed_integer(&field.ty) { reader.exact_sum(column) } else { Ok(None) }
            },
        )
        .collect()
}

fn encode_catalog(entries: &[Entry], views: &[ViewEntry]) -> Result<Vec<u8>> {
    let mut out = CATALOG.to_vec();
    put_u32(&mut out, u32::try_from(entries.len()).map_err(|_| invalid("too many tables"))?);
    for entry in entries {
        let name = entry.name.as_bytes();
        put_u16(&mut out, u16::try_from(name.len()).map_err(|_| invalid("table name too long"))?);
        out.extend_from_slice(name);
        put_u64(&mut out, u64::try_from(entry.rows).map_err(|_| invalid("row count overflow"))?);
        put_u16(
            &mut out,
            u16::try_from(entry.fields.len()).map_err(|_| invalid("too many columns"))?,
        );
        for field in &entry.fields {
            let name = field.name.as_bytes();
            put_u16(
                &mut out,
                u16::try_from(name.len()).map_err(|_| invalid("column name too long"))?,
            );
            out.extend_from_slice(name);
            put_type(&mut out, &field.ty)?;
            out.push(u8::from(field.not_null));
        }
        put_u64(&mut out, entry.directory.offset);
        put_u32(&mut out, entry.directory.length);
        put_u64(&mut out, entry.directory.hash);
    }
    put_u32(&mut out, u32::try_from(views.len()).map_err(|_| invalid("too many views"))?);
    for view in views {
        let name = view.name.as_bytes();
        put_u16(&mut out, u16::try_from(name.len()).map_err(|_| invalid("view name too long"))?);
        out.extend_from_slice(name);
        put_long_text(&mut out, &view.sql, "view body")?;
        put_long_text(&mut out, &view.statement, "view statement")?;
        put_u16(
            &mut out,
            u16::try_from(view.aliases.len()).map_err(|_| invalid("too many aliases"))?,
        );
        for alias in &view.aliases {
            let alias = alias.as_bytes();
            put_u16(
                &mut out,
                u16::try_from(alias.len()).map_err(|_| invalid("alias name too long"))?,
            );
            out.extend_from_slice(alias);
        }
        put_u16(
            &mut out,
            u16::try_from(view.columns.len()).map_err(|_| invalid("too many columns"))?,
        );
        for field in &view.columns {
            let name = field.name.as_bytes();
            put_u16(
                &mut out,
                u16::try_from(name.len()).map_err(|_| invalid("column name too long"))?,
            );
            out.extend_from_slice(name);
            put_type(&mut out, &field.ty)?;
            out.push(u8::from(field.not_null));
        }
    }
    out.extend_from_slice(NONZERO_COUNTS);
    for entry in entries {
        if entry.nonzero.len() != entry.fields.len() {
            return Err(invalid("nonzero count width differs from schema"));
        }
        for count in &entry.nonzero {
            match count {
                None => out.push(0),
                Some(count) => {
                    out.push(1);
                    put_u64(&mut out, *count);
                }
            }
        }
    }
    out.extend_from_slice(AGGREGATE_SUMS);
    for entry in entries {
        if entry.aggregates.len() != entry.fields.len() {
            return Err(invalid("aggregate sum width differs from schema"));
        }
        for summary in &entry.aggregates {
            match summary {
                None => out.push(0),
                Some((sum, count)) => {
                    out.push(1);
                    out.extend_from_slice(&sum.to_le_bytes());
                    put_u64(&mut out, *count);
                }
            }
        }
    }
    out.extend_from_slice(DISTINCT_COUNTS);
    for entry in entries {
        if entry.distincts.len() != entry.fields.len() {
            return Err(invalid("distinct count width differs from schema"));
        }
        for count in &entry.distincts {
            match count {
                None => out.push(0),
                Some(count) => {
                    if *count > entry.rows as u64 {
                        return Err(invalid("distinct count exceeds table rows"));
                    }
                    out.push(1);
                    put_u64(&mut out, *count);
                }
            }
        }
    }
    out.extend_from_slice(INTEGER_EXTREMES);
    for entry in entries {
        if entry.extremes.len() != entry.fields.len() {
            return Err(invalid("integer extremes width differs from schema"));
        }
        for (field, extremes) in entry.fields.iter().zip(&entry.extremes) {
            match extremes {
                None => out.push(0),
                Some(None) if integer_or_date(&field.ty) => out.push(1),
                Some(Some((low, high))) if integer_or_date(&field.ty) && low <= high => {
                    out.push(2);
                    out.extend_from_slice(&low.to_le_bytes());
                    out.extend_from_slice(&high.to_le_bytes());
                }
                _ => return Err(invalid("integer extremes type or range differs")),
            }
        }
    }
    out.extend_from_slice(COMPLETE_FREQUENCIES);
    for entry in entries {
        if entry.frequencies.len() != entry.fields.len() {
            return Err(invalid("numeric frequency width differs from schema"));
        }
        for (field, frequencies) in entry.fields.iter().zip(&entry.frequencies) {
            match frequencies {
                None => out.push(0),
                Some(entries)
                    if integer_or_date(&field.ty) && entries.len() <= MAX_CATALOG_FREQUENCIES =>
                {
                    let mut total = 0_u64;
                    for (at, (value, count)) in entries.iter().enumerate() {
                        if entries[..at].iter().any(|(held, _)| held == value) {
                            return Err(invalid("numeric frequency value repeats"));
                        }
                        total = total
                            .checked_add(*count)
                            .ok_or_else(|| invalid("numeric frequency count overflows"))?;
                    }
                    if total != entry.rows as u64 {
                        return Err(invalid("numeric frequencies do not cover table rows"));
                    }
                    out.push(1);
                    out.push(entries.len() as u8);
                    for (value, count) in entries {
                        match value {
                            None => out.push(0),
                            Some(value) => {
                                out.push(1);
                                out.extend_from_slice(&value.to_le_bytes());
                            }
                        }
                        put_u64(&mut out, *count);
                    }
                }
                _ => return Err(invalid("numeric frequency type or width differs")),
            }
        }
    }
    Ok(out)
}

/// A length and that many bytes, for text that is allowed to be longer than a name.
fn put_long_text(out: &mut Vec<u8>, text: &str, what: &str) -> Result<()> {
    let bytes = text.as_bytes();
    put_u32(out, u32::try_from(bytes.len()).map_err(|_| invalid(&format!("{what} too long")))?);
    out.extend_from_slice(bytes);
    Ok(())
}

/// Reads the catalog directory back, checking every span against the file before anything is
/// allocated for it.
fn decode_catalog(bytes: &[u8], size: u64) -> Result<(Vec<Entry>, Vec<ViewEntry>)> {
    let mut cur = Cursor::new(bytes);
    if cur.take(8)? != CATALOG {
        return Err(invalid("catalog magic differs"));
    }
    let count = cur.u32()? as usize;
    let mut entries: Vec<Entry> = Vec::with_capacity(count.min(1024));
    for _ in 0..count {
        let name = cur.text()?;
        let rows = usize::try_from(cur.u64()?).map_err(|_| invalid("row count does not fit"))?;
        let width = cur.u16()? as usize;
        let mut fields = Vec::with_capacity(width);
        for _ in 0..width {
            let name = cur.text()?;
            let ty = read_type(&mut cur)?;
            let not_null = match cur.u8()? {
                0 => false,
                1 => true,
                _ => return Err(invalid("nullability flag differs")),
            };
            fields.push(Field { name, ty, not_null });
        }
        let directory = Page { offset: cur.u64()?, length: cur.u32()?, hash: cur.u64()? };
        let end = directory
            .offset
            .checked_add(u64::from(directory.length))
            .ok_or_else(|| invalid("table directory offset overflow"))?;
        if directory.offset < HEADER
            || end > size
            || directory.length as usize > MAX_DIRECTORY
            || directory.length == 0
        {
            return Err(invalid("table directory range is outside the file"));
        }
        if entries.iter().any(|held| held.name == name) {
            return Err(invalid("two tables in the catalog have the same name"));
        }
        let nonzero = vec![None; fields.len()];
        let aggregates = vec![None; fields.len()];
        let distincts = vec![None; fields.len()];
        let extremes = vec![None; fields.len()];
        let frequencies = vec![None; fields.len()];
        entries.push(Entry {
            name,
            fields,
            rows,
            directory,
            nonzero,
            aggregates,
            distincts,
            extremes,
            frequencies,
        });
    }
    // A catalog that ends where the tables end is a catalog with no views in it, which is every
    // file written before format 25. That is why the count is allowed to be missing rather than
    // read as a zero that has to be there: an older file has nothing after the last table entry at
    // all, and [`READABLE`] says those files still open.
    let count = if cur.done() { 0 } else { cur.u32()? as usize };
    let mut views: Vec<ViewEntry> = Vec::with_capacity(count.min(1024));
    for _ in 0..count {
        let name = cur.text()?;
        let sql = cur.long_text()?;
        let statement = cur.long_text()?;
        let width = cur.u16()? as usize;
        let mut aliases = Vec::with_capacity(width);
        for _ in 0..width {
            aliases.push(cur.text()?);
        }
        let width = cur.u16()? as usize;
        let mut columns = Vec::with_capacity(width);
        for _ in 0..width {
            let name = cur.text()?;
            let ty = read_type(&mut cur)?;
            let not_null = match cur.u8()? {
                0 => false,
                1 => true,
                _ => return Err(invalid("nullability flag differs")),
            };
            columns.push(Field { name, ty, not_null });
        }
        // The same rule the tables above get, and for the same reason. Two entries under one name
        // is a catalog nothing can answer a lookup from, and finding that out here is better than
        // finding it out from whichever of the two a search happened to reach first.
        if views.iter().any(|held| held.name == name) {
            return Err(invalid("two views in the catalog have the same name"));
        }
        if entries.iter().any(|held| held.name == name) {
            return Err(invalid("a table and a view in the catalog have the same name"));
        }
        views.push(ViewEntry { name, sql, statement, aliases, columns });
    }
    if !cur.done() {
        if cur.take(8)? != NONZERO_COUNTS {
            return Err(invalid("catalog extension magic differs"));
        }
        for entry in &mut entries {
            for (field, count) in entry.fields.iter().zip(&mut entry.nonzero) {
                *count = match cur.u8()? {
                    0 => None,
                    1 if matches!(
                        field.ty,
                        LogicalType::TinyInt
                            | LogicalType::SmallInt
                            | LogicalType::Integer
                            | LogicalType::BigInt
                            | LogicalType::UTinyInt
                            | LogicalType::USmallInt
                            | LogicalType::UInteger
                            | LogicalType::UBigInt
                    ) =>
                    {
                        let value = cur.u64()?;
                        if value > entry.rows as u64 {
                            return Err(invalid("nonzero count exceeds rows"));
                        }
                        Some(value)
                    }
                    _ => return Err(invalid("nonzero count tag or column type differs")),
                };
            }
        }
    }
    if !cur.done() {
        if cur.take(8)? != AGGREGATE_SUMS {
            return Err(invalid("aggregate catalog extension magic differs"));
        }
        for entry in &mut entries {
            for (field, summary) in entry.fields.iter().zip(&mut entry.aggregates) {
                *summary = match cur.u8()? {
                    0 => None,
                    1 if signed_integer(&field.ty) => {
                        let sum = i128::from_le_bytes(
                            cur.take(16)?
                                .try_into()
                                .map_err(|_| invalid("aggregate sum is truncated"))?,
                        );
                        let count = cur.u64()?;
                        if count > entry.rows as u64 {
                            return Err(invalid("aggregate count exceeds table rows"));
                        }
                        Some((sum, count))
                    }
                    _ => return Err(invalid("aggregate sum tag or column type differs")),
                };
            }
        }
    }
    if !cur.done() {
        if cur.take(8)? != DISTINCT_COUNTS {
            return Err(invalid("distinct catalog extension magic differs"));
        }
        for entry in &mut entries {
            for count in &mut entry.distincts {
                *count = match cur.u8()? {
                    0 => None,
                    1 => {
                        let value = cur.u64()?;
                        if value > entry.rows as u64 {
                            return Err(invalid("distinct count exceeds table rows"));
                        }
                        Some(value)
                    }
                    _ => return Err(invalid("distinct count tag differs")),
                };
            }
        }
    }
    if !cur.done() {
        if cur.take(8)? != INTEGER_EXTREMES {
            return Err(invalid("integer extremes catalog extension magic differs"));
        }
        for entry in &mut entries {
            for (field, extremes) in entry.fields.iter().zip(&mut entry.extremes) {
                *extremes = match cur.u8()? {
                    0 => None,
                    1 if integer_or_date(&field.ty) => Some(None),
                    2 if integer_or_date(&field.ty) => {
                        let low = i128::from_le_bytes(
                            cur.take(16)?
                                .try_into()
                                .map_err(|_| invalid("minimum is truncated"))?,
                        );
                        let high = i128::from_le_bytes(
                            cur.take(16)?
                                .try_into()
                                .map_err(|_| invalid("maximum is truncated"))?,
                        );
                        if low > high {
                            return Err(invalid("integer extremes are reversed"));
                        }
                        Some(Some((low, high)))
                    }
                    _ => return Err(invalid("integer extremes tag or type differs")),
                };
            }
        }
    }
    if !cur.done() {
        if cur.take(8)? != COMPLETE_FREQUENCIES {
            return Err(invalid("numeric frequency catalog extension magic differs"));
        }
        for entry in &mut entries {
            for (field, frequencies) in entry.fields.iter().zip(&mut entry.frequencies) {
                *frequencies = match cur.u8()? {
                    0 => None,
                    1 if integer_or_date(&field.ty) => {
                        let len = cur.u8()? as usize;
                        if len > MAX_CATALOG_FREQUENCIES {
                            return Err(invalid("too many catalog numeric frequencies"));
                        }
                        let mut values = Vec::with_capacity(len);
                        let mut total = 0_u64;
                        for _ in 0..len {
                            let value = match cur.u8()? {
                                0 => None,
                                1 => Some(i128::from_le_bytes(cur.take(16)?.try_into().map_err(
                                    |_| invalid("numeric frequency value is truncated"),
                                )?)),
                                _ => return Err(invalid("numeric frequency value tag differs")),
                            };
                            if values.iter().any(|(held, _)| *held == value) {
                                return Err(invalid("numeric frequency value repeats"));
                            }
                            let count = cur.u64()?;
                            total = total
                                .checked_add(count)
                                .ok_or_else(|| invalid("numeric frequency count overflows"))?;
                            values.push((value, count));
                        }
                        if total != entry.rows as u64 {
                            return Err(invalid("numeric frequencies do not cover table rows"));
                        }
                        Some(values)
                    }
                    _ => return Err(invalid("numeric frequency tag or type differs")),
                };
            }
        }
    }
    if !cur.done() {
        return Err(invalid("catalog has trailing bytes"));
    }
    Ok((entries, views))
}

/// Reads the fields of a directory or a catalog in order, off bytes in memory or out of the file.
///
/// A catalog is small and is read whole. A table directory is not: at ten million rows of `hits` it
/// is nearly a megabyte, and holding that buffer while the table it describes is built out of it
/// put both at the peak of every query. Out of the file, the cursor holds one window of
/// [`DIRECTORY_WINDOW`] bytes and moves it forward as the fields are read, so what a directory
/// costs at open is what it decodes into and not that plus its own bytes.
struct Cursor<'a> {
    bytes: &'a [u8],
    at: usize,
    window: Option<Window<'a>>,
}

/// The part of a directory in the file that a [`Cursor`] has read in.
struct Window<'a> {
    file: &'a File,
    offset: u64,
    length: usize,
    /// Where `held` starts, counted from the start of the directory.
    start: usize,
    held: Vec<u8>,
    /// How much to read at once, which is [`DIRECTORY_WINDOW`] outside the tests.
    size: usize,
}

/// How much of a directory a cursor reading one out of the file holds at once.
const DIRECTORY_WINDOW: usize = 64 << 10;

impl<'a> Cursor<'a> {
    fn new(bytes: &'a [u8]) -> Self {
        Self { bytes, at: 0, window: None }
    }

    /// A cursor over `length` bytes of `file` from `offset`, which it reads a window at a time.
    fn over(file: &'a File, offset: u64, length: usize) -> Self {
        let window =
            Window { file, offset, length, start: 0, held: Vec::new(), size: DIRECTORY_WINDOW };
        Self { bytes: &[], at: 0, window: Some(window) }
    }

    /// How many bytes the cursor walks in all.
    fn len(&self) -> usize {
        self.window.as_ref().map_or(self.bytes.len(), |window| window.length)
    }

    /// Makes sure the next `len` bytes are in memory.
    fn ensure(&mut self, len: usize) -> Result<()> {
        let end = self.at.checked_add(len).ok_or_else(|| invalid("directory offset overflow"))?;
        if end > self.len() {
            return Err(invalid("directory is truncated"));
        }
        let Some(window) = &mut self.window else { return Ok(()) };
        if self.at < window.start || end > window.start + window.held.len() {
            let want = len.max(window.size).min(window.length - self.at);
            window.start = self.at;
            window.held.resize(want, 0);
            read_at(window.file, window.offset + self.at as u64, &mut window.held)?;
        }
        Ok(())
    }

    /// `len` bytes from `at`, which [`Self::ensure`] has already brought in.
    fn held(&self, at: usize, len: usize) -> &[u8] {
        match &self.window {
            Some(window) => &window.held[at - window.start..at - window.start + len],
            None => &self.bytes[at..at + len],
        }
    }

    /// The next `len` bytes, without moving past them.
    #[inline]
    fn peek(&mut self, len: usize) -> Result<&[u8]> {
        if self.window.is_none() {
            let bytes = self.bytes;
            return Ok(&bytes[self.at..self.end(len)?]);
        }
        self.ensure(len)?;
        Ok(self.held(self.at, len))
    }

    /// The next `len` bytes, moving past them.
    ///
    /// Every data page is decoded through this, a byte or a word at a time, so a cursor over bytes
    /// already in memory takes them here and never reaches [`Self::ensure`]. With the window check
    /// on every call, q06 on TPC-H spent a seventh of its instructions in it.
    #[inline]
    fn take(&mut self, len: usize) -> Result<&[u8]> {
        if self.window.is_none() {
            let bytes = self.bytes;
            let (at, end) = (self.at, self.end(len)?);
            self.at = end;
            return Ok(&bytes[at..end]);
        }
        self.take_windowed(len)
    }

    /// Moves over a checked field without reading its payload from a windowed directory.
    fn skip(&mut self, len: usize) -> Result<()> {
        let end = self.at.checked_add(len).ok_or_else(|| invalid("directory offset overflow"))?;
        if end > self.len() {
            return Err(invalid("directory is truncated"));
        }
        self.at = end;
        Ok(())
    }

    fn skip_bound(&mut self) -> Result<()> {
        match self.u8()? {
            0 => Ok(()),
            1 => self.skip(16),
            2 => self.skip(8),
            3 => {
                let length = self.u32()? as usize;
                self.skip(length)
            }
            4 => self.skip(17),
            _ => Err(invalid("a stored bound has an unknown tag")),
        }
    }

    /// Where `len` bytes from here end, when they end inside the bytes.
    #[inline]
    fn end(&self, len: usize) -> Result<usize> {
        let end = self.at.checked_add(len).ok_or_else(|| invalid("directory offset overflow"))?;
        if end > self.bytes.len() {
            return Err(invalid("directory is truncated"));
        }
        Ok(end)
    }

    /// [`Self::take`] out of the file, a window at a time.
    #[inline(never)]
    fn take_windowed(&mut self, len: usize) -> Result<&[u8]> {
        self.ensure(len)?;
        self.at += len;
        Ok(self.held(self.at - len, len))
    }
    #[inline]
    fn u8(&mut self) -> Result<u8> {
        Ok(self.take(1)?[0])
    }
    #[inline]
    fn u16(&mut self) -> Result<u16> {
        Ok(u16::from_le_bytes(self.take(2)?.try_into().expect("two bytes")))
    }
    #[inline]
    fn u32(&mut self) -> Result<u32> {
        Ok(u32::from_le_bytes(self.take(4)?.try_into().expect("four bytes")))
    }
    #[inline]
    fn u64(&mut self) -> Result<u64> {
        Ok(u64::from_le_bytes(self.take(8)?.try_into().expect("eight bytes")))
    }
    fn var_u64(&mut self) -> Result<u64> {
        let mut value = 0_u64;
        for shift in (0..=63).step_by(7) {
            let byte = self.u8()?;
            let part = u64::from(byte & 0x7f);
            if shift == 63 && part > 1 {
                return Err(invalid("frequency ordinal varint overflows"));
            }
            value |= part << shift;
            if byte & 0x80 == 0 {
                return Ok(value);
            }
        }
        Err(invalid("frequency ordinal varint is too long"))
    }
    /// A zone map's end, in the layout `rudb_common::bounds` defines.
    ///
    /// The bytes are the ones this directory has written since format 10 and the codec moved to
    /// rank zero rather than being copied, because a column summary now writes the same two ends
    /// and two encodings of one type is how the two quietly stop agreeing.
    ///
    /// A bound's length is in the bound, so out of the file the cursor offers the codec a few bytes
    /// and offers it twice as many whenever it runs out before the directory does.
    fn bound(&mut self) -> Result<Option<Bound>> {
        let rest = self.len().saturating_sub(self.at);
        let mut want = 32;
        loop {
            let offered = self.peek(want.min(rest))?;
            let mut used = 0;
            match bounds::get(offered, &mut used) {
                Ok(bound) => {
                    self.at += used;
                    return Ok(bound);
                }
                Err(_) if want < rest => want *= 2,
                Err(error) => return Err(error),
            }
        }
    }
    fn text(&mut self) -> Result<String> {
        let len = self.u16()? as usize;
        String::from_utf8(self.take(len)?.to_vec()).map_err(|_| invalid("name is not UTF-8"))
    }
    /// Whether everything has been read, which is how a section that an older file does not have at
    /// all is told from one that is there and empty.
    fn done(&self) -> bool {
        self.at >= self.len()
    }
    /// The same, for text that is a query rather than a name.
    ///
    /// A name fits in sixteen bits of length and a view body does not have to. Nobody writes a 64
    /// kilobyte identifier by accident and people do write generated queries that long, and a view
    /// that could not be written down because its body was too big would be a limit invented here
    /// rather than one anything else in the engine has.
    fn long_text(&mut self) -> Result<String> {
        let len = self.u32()? as usize;
        String::from_utf8(self.take(len)?.to_vec()).map_err(|_| invalid("text is not UTF-8"))
    }
}

/// One column's frequency synopsis, or `None` for a column that has none, checked against the column.
fn decode_summary(
    cur: &mut Cursor<'_>,
    field: &Field,
    rows: usize,
    values: bool,
) -> Result<Option<FrequencySummary>> {
    Ok(match cur.u8()? {
        0 => None,
        1 => {
            let omitted_max = cur.u64()?;
            let count = cur.u32()? as usize;
            if count > FREQUENCY_ENTRIES {
                return Err(invalid("frequency entry count exceeds its bound"));
            }
            let mut entries = Vec::with_capacity(count);
            // row at a time: directory decoding validates each persisted bounded frequency entry.
            for _ in 0..count {
                let value = match cur.u8()? {
                    0 => FrequencyValue::Null,
                    1 => FrequencyValue::Integer(i128::from_le_bytes(
                        cur.take(16)?.try_into().expect("sixteen bytes"),
                    )),
                    2 => FrequencyValue::Code(cur.u32()?),
                    _ => return Err(invalid("frequency value tag differs")),
                };
                let valid = matches!(
                    (&field.ty, value),
                    (_, FrequencyValue::Null)
                        | (LogicalType::Varchar | LogicalType::Blob, FrequencyValue::Code(_))
                        | (
                            LogicalType::TinyInt
                                | LogicalType::SmallInt
                                | LogicalType::Integer
                                | LogicalType::BigInt
                                | LogicalType::UTinyInt
                                | LogicalType::USmallInt
                                | LogicalType::UInteger
                                | LogicalType::UBigInt
                                | LogicalType::Date
                                | LogicalType::Timestamp,
                            FrequencyValue::Integer(_),
                        )
                );
                if !valid {
                    return Err(invalid("frequency value does not match its column"));
                }
                let count = cur.u64()?;
                if count == 0 || count > rows as u64 {
                    return Err(invalid("frequency count is outside the table"));
                }
                entries.push(FrequencyEntry { value, count });
            }
            if entries.windows(2).any(|pair| pair[0].count < pair[1].count) {
                return Err(invalid("frequency entries are not descending"));
            }
            let ordinals = {
                let ordinal_count = cur.u32()? as usize;
                if ordinal_count > FREQUENCY_ORDINALS || ordinal_count > rows {
                    return Err(invalid("frequency ordinal count exceeds its bound"));
                }
                let mut ordinals = Vec::with_capacity(ordinal_count);
                let mut previous = 0_u64;
                for at in 0..ordinal_count {
                    let delta = cur.var_u64()?;
                    if at != 0 && delta == 0 {
                        return Err(invalid("frequency ordinals are not increasing"));
                    }
                    let ordinal = if at == 0 {
                        delta
                    } else {
                        previous
                            .checked_add(delta)
                            .ok_or_else(|| invalid("frequency ordinal overflows"))?
                    };
                    if ordinal >= rows as u64 {
                        return Err(invalid("frequency ordinal is outside the table"));
                    }
                    ordinals.push(ordinal);
                    previous = ordinal;
                }
                ordinals
            };
            let ordinal_entries = if values {
                let mut ordinal_entries = Vec::with_capacity(ordinals.len());
                for _ in 0..ordinals.len() {
                    let entry = cur.u16()?;
                    if entry as usize >= entries.len() {
                        return Err(invalid("frequency ordinal value is outside its entries"));
                    }
                    ordinal_entries.push(entry);
                }
                ordinal_entries
            } else {
                Vec::new()
            };
            Some(FrequencySummary { entries, omitted_max, ordinals, ordinal_entries })
        }
        _ => return Err(invalid("frequency summary tag differs")),
    })
}

/// Skips a synopsis whose column the caller does not need. The directory checksum was checked
/// before this walk, and the fields still need their lengths and tags checked to find the next one.
fn skip_summary(cur: &mut Cursor<'_>, values: bool, rows: usize) -> Result<()> {
    match cur.u8()? {
        0 => Ok(()),
        1 => {
            cur.skip(8)?;
            let entries = cur.u32()? as usize;
            if entries > FREQUENCY_ENTRIES {
                return Err(invalid("frequency entry count exceeds its bound"));
            }
            for _ in 0..entries {
                match cur.u8()? {
                    0 => {}
                    1 => cur.skip(16)?,
                    2 => cur.skip(4)?,
                    _ => return Err(invalid("frequency value tag differs")),
                }
                cur.skip(8)?;
            }
            let ordinals = cur.u32()? as usize;
            if ordinals > FREQUENCY_ORDINALS || ordinals > rows {
                return Err(invalid("frequency ordinal count exceeds its bound"));
            }
            for _ in 0..ordinals {
                cur.var_u64()?;
            }
            if values {
                cur.skip(ordinals * 2)?;
            }
            Ok(())
        }
        _ => Err(invalid("frequency summary tag differs")),
    }
}

/// Reads only the catalog, stripe null counts, and one frequency synopsis. This is the cold path
/// for a summary-backed count; constructing every page descriptor and zone map would make it cost
/// the size of the table directory even when no row is read.
fn quick_nonzero(
    mut cur: Cursor<'_>,
    name: &str,
    fields: &[Field],
    rows: usize,
    wanted: usize,
) -> Result<Option<u64>> {
    if cur.take(8)? != DIRECTORY || cur.text()? != name {
        return Err(invalid("table directory differs from the catalog"));
    }
    let width = cur.u16()? as usize;
    if width != fields.len() {
        return Err(invalid("table directory width differs from the catalog"));
    }
    for field in fields {
        let stored =
            Field { name: cur.text()?, ty: read_type(&mut cur)?, not_null: cur.u8()? != 0 };
        if &stored != field {
            return Err(invalid("table directory schema differs from the catalog"));
        }
    }
    let mut dictionaries = Vec::with_capacity(width);
    for field in fields {
        let held = match cur.u8()? {
            0 => false,
            tag if coded_type(&field.ty) && tag == dictionary_tag(&field.ty) => {
                cur.skip(20)?;
                true
            }
            _ => return Err(invalid("dictionary page tag differs")),
        };
        dictionaries.push(held);
    }
    for _ in 0..width {
        match cur.u8()? {
            0 => {}
            1 => cur.skip(8)?,
            _ => return Err(invalid("distinct count tag differs")),
        }
    }
    if cur.u64()? != rows as u64 {
        return Err(invalid("table row count differs from the catalog"));
    }
    let stripes = cur.u32()? as usize;
    let mut total = 0_usize;
    let mut nulls = 0_u64;
    for _ in 0..stripes {
        let parts = cur.u32()? as usize;
        if parts == 0 || parts > STRIPE_PARTS {
            return Err(invalid("stripe part count is outside its bound"));
        }
        let mut stripe_rows = 0_usize;
        for _ in 0..parts {
            stripe_rows = stripe_rows
                .checked_add(cur.u32()? as usize)
                .ok_or_else(|| invalid("stripe row count overflow"))?;
        }
        total =
            total.checked_add(stripe_rows).ok_or_else(|| invalid("stripe row count overflow"))?;
        cur.skip(12 + width * 12)?;
        for (field, held) in fields.iter().zip(&dictionaries) {
            if coded_type(&field.ty) && *held {
                cur.skip(20)?;
            }
        }
        for _ in 0..width * 2 {
            match cur.u8()? {
                0 => {}
                1 => cur.skip(20)?,
                _ => return Err(invalid("stripe page tag differs")),
            }
        }
        for column in 0..width {
            cur.skip_bound()?;
            cur.skip_bound()?;
            let count = cur.u32()? as u64;
            if count > stripe_rows as u64 {
                return Err(invalid("null count exceeds stripe rows"));
            }
            if column == wanted {
                nulls = nulls.checked_add(count).ok_or_else(|| invalid("null count overflow"))?;
            }
            cur.skip(1)?;
            match cur.u8()? {
                0 => {}
                1 => cur.skip(16)?,
                _ => return Err(invalid("a stripe sum has an unknown tag")),
            }
        }
    }
    if total != rows {
        return Err(invalid("table row count differs from stripes"));
    }
    if cur.done() {
        return Ok(None);
    }
    let magic = cur.take(8)?;
    let values = magic == FREQUENCIES;
    if !values && magic != FREQUENCIES_V2 {
        return Err(invalid("directory extension magic differs"));
    }
    if cur.u16()? as usize != width {
        return Err(invalid("frequency column count differs"));
    }
    for _ in 0..wanted {
        skip_summary(&mut cur, values, rows)?;
    }
    let Some(summary) = decode_summary(&mut cur, &fields[wanted], rows, values)? else {
        return Ok(None);
    };
    let zero = summary
        .entries
        .iter()
        .find(|entry| entry.value == FrequencyValue::Integer(0))
        .map(|entry| entry.count)
        .or_else(|| (summary.omitted_max == 0).then_some(0));
    Ok(zero.and_then(|zero| (rows as u64).checked_sub(nulls)?.checked_sub(zero)))
}

/// Walks the row-oriented directory while retaining only one column's index and page spans.
/// The catalog supplies the schema and the caller checks the complete directory checksum first.
fn quick_integer_fold(
    file: &File,
    mut cur: Cursor<'_>,
    entry: &Entry,
    size: u64,
    wanted: usize,
    emit: &mut impl FnMut(i64, u64) -> Result<()>,
) -> Result<()> {
    let name = &entry.name;
    let fields = &entry.fields;
    let rows = entry.rows;
    if cur.take(8)? != DIRECTORY || cur.text()? != name.as_str() {
        return Err(invalid("table directory differs from the catalog"));
    }
    let width = cur.u16()? as usize;
    if width != fields.len() {
        return Err(invalid("table directory width differs from the catalog"));
    }
    for field in fields {
        let stored =
            Field { name: cur.text()?, ty: read_type(&mut cur)?, not_null: cur.u8()? != 0 };
        if &stored != field {
            return Err(invalid("table directory schema differs from the catalog"));
        }
    }
    let mut dictionaries = Vec::with_capacity(width);
    for field in fields {
        dictionaries.push(match cur.u8()? {
            0 => false,
            tag if coded_type(&field.ty) && tag == dictionary_tag(&field.ty) => {
                cur.skip(20)?;
                true
            }
            _ => return Err(invalid("dictionary page tag differs")),
        });
    }
    for _ in 0..width {
        match cur.u8()? {
            0 => {}
            1 => cur.skip(8)?,
            _ => return Err(invalid("distinct count tag differs")),
        }
    }
    if cur.u64()? != rows as u64 {
        return Err(invalid("table row count differs from the catalog"));
    }
    let stripes = cur.u32()? as usize;
    let mut total = 0_usize;
    let mut bytes = Vec::new();
    for _ in 0..stripes {
        let parts = cur.u32()? as usize;
        if parts == 0 || parts > STRIPE_PARTS {
            return Err(invalid("stripe part count is outside its bound"));
        }
        let mut part_rows = Vec::with_capacity(parts);
        for _ in 0..parts {
            let count = cur.u32()? as usize;
            if count == 0 {
                return Err(invalid("empty part"));
            }
            total = total.checked_add(count).ok_or_else(|| invalid("stripe row count overflow"))?;
            part_rows.push(count);
        }
        let index = Span { offset: cur.u64()?, length: cur.u32()? };
        let section = index_section(parts)?;
        let index_length =
            section.checked_mul(width).ok_or_else(|| invalid("index page length overflow"))?;
        if index.offset < HEADER
            || index.offset.checked_add(u64::from(index.length)).is_none_or(|end| end > size)
            || index.length as usize != index_length
        {
            return Err(invalid("index page range is outside the file"));
        }
        cur.skip(wanted * 12)?;
        let page = Span { offset: cur.u64()?, length: cur.u32()? };
        if page.offset < HEADER
            || page.offset.checked_add(u64::from(page.length)).is_none_or(|end| end > size)
            || page.length as usize > MAX_PAGE
        {
            return Err(invalid("column page range is outside the file"));
        }
        cur.skip((width - wanted - 1) * 12)?;
        for (field, held) in fields.iter().zip(&dictionaries) {
            if coded_type(&field.ty) && *held {
                cur.skip(20)?;
            }
        }
        for _ in 0..width * 2 {
            match cur.u8()? {
                0 => {}
                1 => cur.skip(20)?,
                _ => return Err(invalid("stripe page tag differs")),
            }
        }
        for _ in 0..width {
            cur.skip_bound()?;
            cur.skip_bound()?;
            cur.skip(5)?;
            match cur.u8()? {
                0 => {}
                1 => cur.skip(16)?,
                _ => return Err(invalid("a stripe sum has an unknown tag")),
            }
        }
        let spans = read_index_span(file, index, page, parts, wanted)?;
        for (span, expected_rows) in spans.into_iter().zip(part_rows) {
            bytes.resize(span.length, 0);
            let at = page
                .offset
                .checked_add(span.start as u64)
                .ok_or_else(|| invalid("part range overflow"))?;
            read_at(file, at, &mut bytes)?;
            if checksum(&bytes) != span.hash {
                return Err(invalid("integer part checksum differs"));
            }
            if bytes.first() == Some(&5) && bytes.get(1) == Some(&0) {
                let decoded_rows = integer::fold(&bytes[2..], |value, count| {
                    check_integer_tally_value(value, &fields[wanted].ty)?;
                    emit(value, count)
                })?;
                if decoded_rows != expected_rows {
                    return Err(invalid("encoded integer part holds the wrong number of rows"));
                }
            } else {
                let column = decode(&fields[wanted].ty, expected_rows, &bytes, None)?;
                if let Some(packed) = column.packed_parts() {
                    let validity = column.validity();
                    let all_valid = column.none_null();
                    let base = packed.base();
                    let mut codes = [0_u64; 64];
                    for from in (0..expected_rows).step_by(codes.len()) {
                        let count = (expected_rows - from).min(codes.len());
                        packed.unpack(from, &mut codes[..count]);
                        for (offset, &code) in codes[..count].iter().enumerate() {
                            if all_valid || validity.is_valid(from + offset) {
                                // Vector::packed checked that this entire range fits the type.
                                emit((base + i128::from(code)) as i64, 1)?;
                            }
                        }
                    }
                    continue;
                }
                let column = column.into_flat()?;
                let validity = column.validity();
                macro_rules! count_decoded {
                    ($values:expr) => {
                        for (row, &value) in $values.as_slice().iter().enumerate() {
                            if validity.is_valid(row) {
                                emit(i64::from(value), 1)?;
                            }
                        }
                    };
                }
                match column.data() {
                    Some(Data::Int8(values)) => count_decoded!(values),
                    Some(Data::Int16(values)) => count_decoded!(values),
                    Some(Data::Int32(values)) => count_decoded!(values),
                    Some(Data::Int64(values)) => count_decoded!(values),
                    _ => return Err(invalid("decoded integer part has the wrong type")),
                }
            }
        }
    }
    if total != rows {
        return Err(invalid("table row count differs from stripes"));
    }
    Ok(())
}

fn check_integer_tally_value(value: i64, ty: &LogicalType) -> Result<()> {
    let fits = match ty {
        LogicalType::TinyInt => i8::try_from(value).is_ok(),
        LogicalType::SmallInt => i16::try_from(value).is_ok(),
        LogicalType::Integer => i32::try_from(value).is_ok(),
        LogicalType::BigInt => true,
        _ => false,
    };
    if fits { Ok(()) } else { Err(invalid("encoded integer value is outside its column type")) }
}

fn decode_directory(bytes: &[u8], size: u64) -> Result<Table> {
    read_directory(Cursor::new(bytes), size, None)
}

/// A directory out of `cur`, which is a whole one in memory or one being read out of the file.
///
/// `stored_at` is where the directory starts in the file when it is being read out of it, and then
/// every frequency synopsis is checked and left there, as [`Frequencies::Stored`].
fn read_directory(mut cur: Cursor<'_>, size: u64, stored_at: Option<u64>) -> Result<Table> {
    if cur.take(8)? != DIRECTORY {
        return Err(invalid("directory magic differs"));
    }
    let name = cur.text()?;
    let width = cur.u16()? as usize;
    let mut fields = Vec::with_capacity(width);
    for _ in 0..width {
        let name = cur.text()?;
        let ty = read_type(&mut cur)?;
        let not_null = match cur.u8()? {
            0 => false,
            1 => true,
            _ => return Err(invalid("nullability flag differs")),
        };
        fields.push(Field { name, ty, not_null });
    }
    let mut dictionaries = Vec::with_capacity(width);
    for field in &fields {
        dictionaries.push(match cur.u8()? {
            0 => None,
            tag if tag == dictionary_tag(&field.ty) => {
                let page = Page { offset: cur.u64()?, length: cur.u32()?, hash: cur.u64()? };
                let end = page
                    .offset
                    .checked_add(u64::from(page.length))
                    .ok_or_else(|| invalid("dictionary page offset overflow"))?;
                // A global dictionary covers a whole column, not one bounded stripe. Its lazy
                // payload is intentionally allowed to grow past `MAX_PAGE`; only ordinary column
                // pages are capped there. `Writer::finish` has already bounded this length by the
                // on-disk `u32`, and the range check below keeps it inside the file.
                if page.offset < HEADER || end > size {
                    return Err(invalid("dictionary page range is outside the file"));
                }
                Some(page)
            }
            _ => return Err(invalid("dictionary page tag differs")),
        });
    }
    let mut distincts = Vec::with_capacity(width);
    for _ in 0..width {
        distincts.push(match cur.u8()? {
            0 => None,
            1 => Some(cur.u64()?),
            _ => return Err(invalid("distinct count tag differs")),
        });
    }
    let rows = usize::try_from(cur.u64()?).map_err(|_| invalid("row count does not fit"))?;
    let count = cur.u32()? as usize;
    let mut stripes = Vec::with_capacity(count);
    let mut total = 0_usize;
    for _ in 0..count {
        let count = cur.u32()? as usize;
        if count == 0 || count > STRIPE_PARTS {
            return Err(invalid("stripe part count is outside its bound"));
        }
        let mut parts = Vec::with_capacity(count);
        let mut stripe_rows = 0_usize;
        for _ in 0..count {
            let rows = cur.u32()?;
            if rows == 0 {
                return Err(invalid("empty part"));
            }
            parts.push(rows);
            stripe_rows = stripe_rows
                .checked_add(rows as usize)
                .ok_or_else(|| invalid("stripe row count overflow"))?;
        }
        total =
            total.checked_add(stripe_rows).ok_or_else(|| invalid("stripe row count overflow"))?;
        let index = Span { offset: cur.u64()?, length: cur.u32()? };
        let section = index_section(count)?;
        let wanted = section
            .checked_mul(width)
            .and_then(|bytes| u32::try_from(bytes).ok())
            .ok_or_else(|| invalid("index page length overflow"))?;
        let end = index
            .offset
            .checked_add(u64::from(index.length))
            .ok_or_else(|| invalid("index page offset overflow"))?;
        if index.offset < HEADER || end > size || index.length != wanted {
            return Err(invalid("index page range is outside the file"));
        }
        let mut pages = Vec::with_capacity(width);
        for _ in 0..width {
            let offset = cur.u64()?;
            let length = cur.u32()?;
            let end = offset
                .checked_add(u64::from(length))
                .ok_or_else(|| invalid("page offset overflow"))?;
            if offset < HEADER || end > size || length as usize > MAX_PAGE {
                return Err(invalid("page range is outside the file"));
            }
            pages.push(Span { offset, length });
        }
        let mut memberships = vec![None; width];
        for (column, field) in fields.iter().enumerate() {
            if !coded_type(&field.ty) || dictionaries[column].is_none() {
                continue;
            }
            let page = Page { offset: cur.u64()?, length: cur.u32()?, hash: cur.u64()? };
            let end = page
                .offset
                .checked_add(u64::from(page.length))
                .ok_or_else(|| invalid("membership page offset overflow"))?;
            if page.offset < HEADER || end > size || page.length as usize > MAX_PAGE {
                return Err(invalid("membership page range is outside the file"));
            }
            // No bytes is a stripe written after the column's dictionary was demoted, see
            // [`DEMOTED`], which is checked once the block that says so has been read.
            if page.length != 0 {
                memberships[column] = Some(page);
            }
        }
        let mut sieves = vec![None; width];
        for sieve in sieves.iter_mut().take(width) {
            match cur.u8()? {
                0 => continue,
                1 => {}
                _ => return Err(invalid("a sieve page has an unknown tag")),
            }
            let page = Page { offset: cur.u64()?, length: cur.u32()?, hash: cur.u64()? };
            let end = page
                .offset
                .checked_add(u64::from(page.length))
                .ok_or_else(|| invalid("sieve page offset overflow"))?;
            if page.offset < HEADER || end > size || page.length as usize > MAX_PAGE {
                return Err(invalid("sieve page range is outside the file"));
            }
            *sieve = Some(page);
        }
        let mut part_ranges = vec![None; width];
        for held in part_ranges.iter_mut().take(width) {
            match cur.u8()? {
                0 => continue,
                1 => {}
                _ => return Err(invalid("a part range page has an unknown tag")),
            }
            let page = Page { offset: cur.u64()?, length: cur.u32()?, hash: cur.u64()? };
            let end = page
                .offset
                .checked_add(u64::from(page.length))
                .ok_or_else(|| invalid("part range page offset overflow"))?;
            if page.offset < HEADER || end > size || page.length as usize > MAX_PAGE {
                return Err(invalid("part range page range is outside the file"));
            }
            *held = Some(page);
        }
        let mut ranges = Vec::with_capacity(width);
        for column in 0..width {
            let low = cur.bound()?;
            let high = cur.bound()?;
            let nulls = cur.u32()? as usize;
            if nulls > stripe_rows {
                return Err(invalid("null count exceeds stripe rows"));
            }
            let exact = cur.u8()? != 0;
            let sum = match cur.u8()? {
                0 => None,
                1 => Some(i128::from_le_bytes(
                    cur.take(16)?.try_into().map_err(|_| invalid("a stripe sum is truncated"))?,
                )),
                _ => return Err(invalid("a stripe sum has an unknown tag")),
            };
            // Files written before the ends of a decimal or a timestamp column carried their power
            // of ten hold a bare integer here, and that integer is the one the column holds, which
            // is what the power is over. So the type puts it back on the way in and an old file
            // prunes as well as a new one. A file that already wrote the power keeps it, because
            // this leaves anything that is not an integer alone.
            let ty = &fields.get(column).ok_or_else(|| invalid("a stripe range has no column"))?.ty;
            let low = low.map(|bound| scaled_as(bound, ty));
            let high = high.map(|bound| scaled_as(bound, ty));
            ranges.push(Range { low, high, nulls, exact, sum });
        }
        stripes.push(Stripe {
            rows: stripe_rows,
            parts,
            index,
            pages,
            memberships: Pages::from_slots(memberships)?,
            sieves: Pages::from_slots(sieves)?,
            part_ranges: Pages::from_slots(part_ranges)?,
            zone: Zone::from_ranges(ranges),
        });
    }
    if total != rows {
        return Err(invalid("table row count differs from stripes"));
    }
    // How many entries each column's synopsis lists, which is all a pair summary is checked against,
    // kept apart because the synopses themselves may be left in the file.
    let mut entry_counts = vec![0; width];
    let frequencies = if cur.done() {
        vec![None; width]
    } else {
        let frequency_magic = cur.take(8)?;
        let frequency_values = frequency_magic == FREQUENCIES;
        if !frequency_values && frequency_magic != FREQUENCIES_V2 {
            return Err(invalid("directory extension magic differs"));
        }
        if cur.u16()? as usize != width {
            return Err(invalid("frequency column count differs"));
        }
        let mut frequencies = Vec::with_capacity(width);
        for (field, entry_count) in fields.iter().zip(&mut entry_counts) {
            let start = cur.at;
            let summary = decode_summary(&mut cur, field, rows, frequency_values)?;
            *entry_count = summary.as_ref().map_or(0, |summary| summary.entries.len());
            frequencies.push(match (summary, stored_at) {
                (None, _) => None,
                (Some(summary), None) => Some(Frequencies::Held(summary)),
                (Some(_), Some(offset)) => Some(Frequencies::Stored {
                    span: Span {
                        offset: offset + start as u64,
                        length: u32::try_from(cur.at - start)
                            .map_err(|_| invalid("a frequency synopsis is too long"))?,
                    },
                    values: frequency_values,
                }),
            });
        }
        frequencies
    };
    // There are two optional trailing blocks now rather than one, so the reader dispatches on the
    // magic it finds rather than on where the bytes ran out. That is what lets the two arrive
    // independently: a format 22 directory ends here and has neither, a directory written before
    // the section table has only the clustering declaration, and each one still opens without a
    // rewrite. It is the G1 exit criterion, which is that a reader that knows about sections opens
    // a file that predates them and answers every query, only without the graph path.
    //
    // A repeated block is refused rather than allowed to win, because two clustering declarations
    // in one directory is a torn directory and the only question is which of them is the lie.
    let mut clustering = None;
    let mut sections = Vec::new();
    let mut pair_frequencies = Vec::new();
    let mut seen_pair_frequencies = false;
    let mut frequency_texts = vec![Vec::new(); width];
    let mut seen_frequency_texts = false;
    let mut host_groups = None;
    let mut demoted = Vec::new();
    let mut seen_sections = false;
    let mut dictionary_payloads = Vec::new();
    let mut seen_payloads = false;
    // Zero until a section table says otherwise, which is what a format 22 table gets and what
    // makes every section stamp fail to match on one, because real generations start at one.
    let mut generation = 0;
    while !cur.done() {
        let mut tag = [0u8; 8];
        tag.copy_from_slice(cur.take(8)?);
        if &tag == PAIR_FREQUENCIES {
            if seen_pair_frequencies {
                return Err(invalid("directory names two pair frequency blocks"));
            }
            seen_pair_frequencies = true;
            let count = cur.u16()? as usize;
            if count > MAX_PAIR_FREQUENCIES {
                return Err(invalid("pair frequency count exceeds its bound"));
            }
            pair_frequencies = Vec::with_capacity(count);
            for _ in 0..count {
                let first = cur.u16()?;
                let second = cur.u16()?;
                let first_at = first as usize;
                let second_at = second as usize;
                if frequencies.get(first_at).and_then(Option::as_ref).is_none() {
                    return Err(invalid("pair frequency first column has no synopsis"));
                }
                let first_entries = entry_counts[first_at];
                if !matches!(fields.get(second_at), Some(field) if field.ty == LogicalType::Varchar)
                    || dictionaries.get(second_at).copied().flatten().is_none()
                {
                    return Err(invalid("pair frequency second column has no stable dictionary"));
                }
                if pair_frequencies
                    .iter()
                    .any(|held: &PairFrequencySummary| held.first == first && held.second == second)
                {
                    return Err(invalid("directory repeats a pair frequency summary"));
                }
                let omitted_max = cur.u64()?;
                if omitted_max > rows as u64 {
                    return Err(invalid("pair frequency omitted count exceeds the table"));
                }
                let entries_count = cur.u16()? as usize;
                if entries_count > FREQUENCY_ENTRIES {
                    return Err(invalid("pair frequency entry count exceeds its bound"));
                }
                let mut entries = Vec::with_capacity(entries_count);
                for _ in 0..entries_count {
                    let first_entry = cur.u16()?;
                    if first_entry as usize >= first_entries {
                        return Err(invalid("pair frequency anchor is outside its synopsis"));
                    }
                    let second = match cur.u8()? {
                        0 => None,
                        1 => Some(cur.u32()?),
                        _ => return Err(invalid("pair frequency string tag differs")),
                    };
                    let count = cur.u64()?;
                    if count == 0 || count > rows as u64 {
                        return Err(invalid("pair frequency count is outside the table"));
                    }
                    entries.push(PairFrequencyEntry { first_entry, second, count });
                }
                if entries.windows(2).any(|pair| pair[0].count < pair[1].count) {
                    return Err(invalid("pair frequency entries are not descending"));
                }
                pair_frequencies.push(PairFrequencySummary { first, second, entries, omitted_max });
            }
        } else if &tag == FREQUENCY_TEXTS {
            if seen_frequency_texts {
                return Err(invalid("directory names two frequency text blocks"));
            }
            seen_frequency_texts = true;
            let columns = cur.u16()? as usize;
            if columns > width {
                return Err(invalid("frequency text column count exceeds the schema"));
            }
            for _ in 0..columns {
                let column = cur.u16()? as usize;
                if !frequency_texts.get(column).is_some_and(Vec::is_empty) {
                    return Err(invalid("frequency text column is repeated or out of range"));
                }
                if !matches!(fields.get(column), Some(field) if coded_type(&field.ty))
                    || dictionaries.get(column).copied().flatten().is_none()
                    || frequencies.get(column).and_then(Option::as_ref).is_none()
                {
                    return Err(invalid("frequency texts belong to a non-string synopsis"));
                }
                let count = cur.u16()? as usize;
                if count == 0 || count != entry_counts[column] {
                    return Err(invalid("frequency text count differs from its synopsis"));
                }
                let mut texts = Vec::with_capacity(count);
                for _ in 0..count {
                    texts.push(match cur.u8()? {
                        0 => None,
                        1 => {
                            let length = cur.u32()? as usize;
                            let bytes = cur.take(length)?.to_vec();
                            if fields[column].ty == LogicalType::Varchar {
                                std::str::from_utf8(&bytes)
                                    .map_err(|_| invalid("frequency text is not UTF-8"))?;
                            }
                            Some(bytes)
                        }
                        _ => return Err(invalid("frequency text tag differs")),
                    });
                }
                frequency_texts[column] = texts;
            }
        } else if &tag == HOST_GROUPS {
            if host_groups.is_some() {
                return Err(invalid("directory names two host group blocks"));
            }
            let column = cur.u16()? as usize;
            if !matches!(fields.get(column), Some(field) if field.ty == LogicalType::Varchar)
                || dictionaries.get(column).copied().flatten().is_none()
            {
                return Err(invalid("host groups belong to a non-string dictionary"));
            }
            let omitted_max = cur.u64()?;
            if omitted_max > rows as u64 {
                return Err(invalid("host group bound exceeds the table"));
            }
            let count = cur.u16()? as usize;
            if count > host::CAPACITY {
                return Err(invalid("host group count exceeds its bound"));
            }
            let mut entries = Vec::with_capacity(count);
            let mut bytes = 0_usize;
            for _ in 0..count {
                let host_len = cur.u32()? as usize;
                bytes =
                    bytes.checked_add(host_len).ok_or_else(|| invalid("host bytes overflow"))?;
                if bytes > host::BYTE_BUDGET {
                    return Err(invalid("host groups exceed their byte budget"));
                }
                let host = std::str::from_utf8(cur.take(host_len)?)
                    .map_err(|_| invalid("host is not UTF-8"))?
                    .to_owned();
                let count = cur.u64()?;
                if count == 0 || count > rows as u64 {
                    return Err(invalid("host group count exceeds the table"));
                }
                let bytes_sum = i128::from_le_bytes(
                    cur.take(16)?
                        .try_into()
                        .map_err(|_| invalid("host length sum is truncated"))?,
                );
                if bytes_sum < 0 {
                    return Err(invalid("host length sum is negative"));
                }
                let minimum_len = cur.u32()? as usize;
                bytes =
                    bytes.checked_add(minimum_len).ok_or_else(|| invalid("host bytes overflow"))?;
                if bytes > host::BYTE_BUDGET {
                    return Err(invalid("host groups exceed their byte budget"));
                }
                let minimum = std::str::from_utf8(cur.take(minimum_len)?)
                    .map_err(|_| invalid("host minimum is not UTF-8"))?
                    .to_owned();
                entries.push(host::HostEntry { host, count, bytes_sum, minimum });
            }
            if entries.windows(2).any(|pair| pair[0].count < pair[1].count)
                || entries.iter().any(|entry| entry.host.is_empty() || entry.minimum.is_empty())
            {
                return Err(invalid("host groups are not in certified order"));
            }
            host_groups = Some(host::HostSummary { column, omitted_max, entries });
        } else if &tag == CLUSTERING {
            if clustering.is_some() {
                return Err(invalid("directory names two clustering declarations"));
            }
            let bucket = Width::from_tag(cur.u8()?)
                .ok_or_else(|| invalid("clustering width tag differs"))?;
            let count = cur.u16()? as usize;
            let mut columns = Vec::with_capacity(count.min(fields.len()));
            for _ in 0..count {
                columns.push(u32::from(cur.u16()?));
            }
            // Through the constructor and not built by hand, so that a file claiming a column the
            // table does not have is caught at open rather than at the first scan that trusted it.
            clustering = Some(Clustering::new(columns, bucket, &fields).map_err(|_| {
                invalid("stored clustering declaration does not match the table it is on")
            })?);
        } else if &tag == DEMOTED {
            if !demoted.is_empty() {
                return Err(invalid("directory names two demoted column blocks"));
            }
            let count = cur.u16()? as usize;
            if count == 0 || count > width {
                return Err(invalid("demoted column count is outside the schema"));
            }
            demoted = vec![false; width];
            for _ in 0..count {
                let column = cur.u16()? as usize;
                if dictionaries.get(column).copied().flatten().is_none() || demoted[column] {
                    return Err(invalid("a demoted column is repeated or has no dictionary"));
                }
                demoted[column] = true;
            }
        } else if &tag == SECTIONS {
            if seen_sections {
                return Err(invalid("directory names two section tables"));
            }
            seen_sections = true;
            generation = cur.u64()?;
            let count = cur.u16()? as usize;
            if count > MAX_SECTIONS {
                return Err(invalid("section count exceeds its bound"));
            }
            sections = Vec::with_capacity(count);
            // entry at a time: a malformed section entry is refused rather than turned into an
            // offset.
            for _ in 0..count {
                sections.push(Section::decode(cur.take(section::ENTRY_BYTES)?)?);
            }
            for held in &sections {
                let Some(end) = held.extent_page.checked_add(u64::from(held.extent_bytes)) else {
                    return Err(invalid("a section's extent table overflows the file"));
                };
                // The bound check is here and not in `section`, because only the caller knows how
                // big the file is. A section pointing past the end is a torn directory, and reading
                // the payload it names would be reading whatever else is at that offset.
                if held.extent_bytes != 0 && (held.extent_page < HEADER || end > size) {
                    return Err(invalid("a section's extent table is outside the file"));
                }
                if held.extents == 0 && held.extent_bytes != 0 {
                    return Err(invalid("a section with no extents names an extent table"));
                }
            }
        } else if &tag == DICTIONARY_PAYLOADS {
            if seen_payloads {
                return Err(invalid("directory names two dictionary payload blocks"));
            }
            seen_payloads = true;
            let count = cur.u16()? as usize;
            if count != fields.len() {
                return Err(invalid("dictionary payload block does not match the table's columns"));
            }
            dictionary_payloads = Vec::with_capacity(count);
            for _ in 0..count {
                let bytes = cur.u64()?;
                if bytes > size {
                    return Err(invalid("a dictionary payload is larger than the file"));
                }
                dictionary_payloads.push(bytes);
            }
        } else {
            return Err(invalid("directory extension magic differs"));
        }
    }
    if !cur.done() {
        return Err(invalid("directory has trailing bytes"));
    }
    for stripe in &stripes {
        for (column, field) in fields.iter().enumerate() {
            if coded_type(&field.ty)
                && dictionaries[column].is_some()
                && stripe.memberships.get(column).is_none()
                && !demoted.get(column).copied().unwrap_or(false)
            {
                return Err(invalid("string page has no code membership index"));
            }
        }
    }
    Ok(Table {
        name,
        fields,
        stripes,
        rows,
        dictionaries,
        dictionary_payloads,
        demoted,
        distincts,
        frequencies,
        pair_frequencies,
        frequency_texts,
        host_groups,
        clustering,
        generation,
        sections,
    })
}

/// A zone map's end, in the layout `rudb_common::bounds` defines. See [`Cursor::bound`].
fn put_bound(out: &mut Vec<u8>, bound: Option<&Bound>) -> Result<()> {
    bounds::put(out, bound)
}

/// Which cascades are worth trying on a run of dictionary codes.
///
/// The exhaustive chooser encodes every candidate at every level of a cascade three deep and keeps
/// the smallest, which on a part of 1024 codes is around a hundred full encodes to decide something
/// three candidates were always going to win. It is the right default for a crate that does not
/// know what it is looking at. Here we do know. Codes are counted from zero in the order the values
/// were first seen, so a part of them is one value, or a narrow band, or a few long runs, and those
/// are constant, frame of reference and run length. Nothing else has ever come first on this data.
///
/// A dictionary of dictionary codes is the one candidate that can never pay, because the codes are
/// already the dictionary, and it is also the most expensive one to try. Below the top level the
/// streams are an RLE's run values and run lengths, which are integers in their own right with no
/// runs left in them, so only the two flat candidates go down there.
///
/// This is size given up for time on purpose, and the ablation is this chooser against
/// [`chooser::EXHAUSTIVE`] on the same file.
#[derive(Debug)]
struct Codes;

impl chooser::Chooser for Codes {
    fn name(&self) -> &'static str {
        "codes"
    }

    fn narrow_strings(
        &self,
        _values: &[&[u8]],
        offered: &[string::Kind],
        _depth: u8,
    ) -> Vec<string::Kind> {
        // Never reached, because nothing here encodes strings through the cascade. The trait asks
        // for it and the honest answer to a question we have no opinion on is the whole list.
        offered.to_vec()
    }

    fn narrow_integers(
        &self,
        _values: &[i64],
        offered: &[integer::Kind],
        depth: u8,
    ) -> Vec<integer::Kind> {
        // The contract is a non empty subset, and a chunk that offers none of the three is a chunk
        // this has no opinion about rather than one that cannot be written.
        narrowed_to(Codes::keep(depth), offered)
    }

    fn considers_integer(&self, kind: integer::Kind, depth: u8) -> bool {
        Codes::keep(depth).contains(&kind)
    }
}

impl Codes {
    fn keep(depth: u8) -> &'static [integer::Kind] {
        if depth == 0 {
            &[integer::Kind::Constant, integer::Kind::Packed, integer::Kind::Rle]
        } else {
            &[integer::Kind::Constant, integer::Kind::Packed]
        }
    }
}

/// The kinds of `offered` that are in `keep`, or all of `offered` when none of them are.
///
/// `Packed` applies to every chunk and both choosers keep it, so the fallback is never taken on a
/// chunk the cascade offers. It is there because the contract is a non empty subset and a chooser
/// that returned nothing would be a chunk that cannot be written. It is also why saying no to a kind
/// in `considers_integer` is safe: a kind that is never offered could only have been kept through
/// this fallback, and the fallback is never reached.
fn narrowed_to(keep: &[integer::Kind], offered: &[integer::Kind]) -> Vec<integer::Kind> {
    let narrowed: Vec<integer::Kind> =
        offered.iter().copied().filter(|kind| keep.contains(kind)).collect();
    if narrowed.is_empty() { offered.to_vec() } else { narrowed }
}

/// Which cascades are worth trying on a part of plain integers.
///
/// Wider than [`Codes`] because the values are not codes and carry whatever shape the column has.
/// A timestamp column climbs, so delta is the one that matters and is the reason this exists at
/// all: three timestamp columns in ClickBench were coming out at exactly eight bytes a row with
/// nothing asked of them. The same three columns are why the stride is here, since a timestamp
/// loaded from a source that recorded whole seconds is microseconds with twenty zero bits under
/// every value. A column that is one value with a handful of exceptions is sparse. What is still
/// left out is the dictionary, for the same reason as in [`Codes`]: it is the most
/// expensive candidate to try and this file already puts the columns that want one through a
/// dictionary of their own before they ever reach here.
#[derive(Debug)]
struct Fixed;

impl chooser::Chooser for Fixed {
    fn name(&self) -> &'static str {
        "fixed"
    }

    fn narrow_strings(
        &self,
        _values: &[&[u8]],
        offered: &[string::Kind],
        _depth: u8,
    ) -> Vec<string::Kind> {
        offered.to_vec()
    }

    fn narrow_integers(
        &self,
        _values: &[i64],
        offered: &[integer::Kind],
        depth: u8,
    ) -> Vec<integer::Kind> {
        narrowed_to(Fixed::keep(depth), offered)
    }

    fn considers_integer(&self, kind: integer::Kind, depth: u8) -> bool {
        Fixed::keep(depth).contains(&kind)
    }
}

impl Fixed {
    fn keep(depth: u8) -> &'static [integer::Kind] {
        if depth == 0 {
            &[
                integer::Kind::Constant,
                integer::Kind::Packed,
                integer::Kind::Delta,
                integer::Kind::Rle,
                integer::Kind::Sparse,
                integer::Kind::Strided,
            ]
        } else {
            &[integer::Kind::Constant, integer::Kind::Packed, integer::Kind::Delta]
        }
    }
}

/// Every value of an integer part as an `i64`, or `None` for a part this cannot widen without
/// losing one.
///
/// `UBIGINT` is the only integer type left out, because half its range does not fit and a page that
/// silently wrapped would be worse than a page that stays plain. Booleans and strings are not
/// integers and have their own ways of being small.
fn widened(data: &Data) -> Option<Vec<i64>> {
    match data {
        Data::Int8(values) => Some(values.iter().map(|value| i64::from(*value)).collect()),
        Data::UInt8(values) => Some(values.iter().map(|value| i64::from(*value)).collect()),
        Data::Int16(values) => Some(values.iter().map(|value| i64::from(*value)).collect()),
        Data::UInt16(values) => Some(values.iter().map(|value| i64::from(*value)).collect()),
        Data::Int32(values) => Some(values.iter().map(|value| i64::from(*value)).collect()),
        Data::UInt32(values) => Some(values.iter().map(|value| i64::from(*value)).collect()),
        Data::Int64(values) => Some(values.to_vec()),
        _ => None,
    }
}

/// An integer type a cascaded page can be read back into, and how to tell whether a value fits.
///
/// This exists so that the check and the conversion can be two loops instead of one. `TryFrom` puts
/// them together, which is the right shape for one value and the wrong one for a page: a fallible
/// conversion a value at a time is a branch a value at a time, the branch decides whether the loop
/// keeps going, and a loop like that is one no compiler will widen.
trait Narrow: Copy {
    /// How wide this type is, and what to add to a value to put its range at the bottom of a `u64`.
    ///
    /// Half the width for a signed type, which is what moves its smallest value to zero, and nothing
    /// for an unsigned one, whose smallest value is already there.
    const BIASED: (u32, u64);

    /// The value narrowed, which the caller has already shown fits.
    fn narrow(value: i64) -> Self;
}

/// The bits of `value` a `T` cannot hold, and zero when the value fits.
///
/// The question is asked this way round because the answers or together. A page fits when every
/// residue in it is zero, so the loop is an or into an accumulator and the decision is one test
/// after it, where asking whether each value is between a floor and a ceiling gives an answer that
/// does not combine and turns into a running minimum and maximum.
///
/// Biasing and shifting is what the answer is made of, rather than anything that reads more like the
/// question, because those are the operations a machine has four of. A 64 bit integer minimum is
/// AVX-512. So is a 64 bit arithmetic shift right, which is how the sign extension this could be
/// written as would have to be done. An add and a logical shift right are AVX2 and are on every
/// machine this runs on, so this is the form that gets four values a cycle instead of one.
///
/// Adding the bias moves the type's range to `0..=2^bits`, wrapping, so everything in range shifts
/// away to nothing and everything outside it leaves something behind. A negative value under an
/// unsigned type is caught by the same shift, because a negative `i64` read as a `u64` is enormous.
#[allow(clippy::cast_sign_loss, reason = "a residue is a bit pattern and not a number")]
fn residue<T: Narrow>(value: i64) -> u64 {
    let (bits, bias) = T::BIASED;
    (value as u64).wrapping_add(bias) >> bits
}

/// Says a primitive integer narrows with `as`, and where the bottom of its range is.
///
/// `as` is a truncation and is the right operation here only because [`fit`] has already found every
/// residue zero, and it is what makes the second loop a narrowing store with no branch in it.
macro_rules! narrows {
    ($($ty:ty => $bias:expr),* $(,)?) => {$(
        impl Narrow for $ty {
            const BIASED: (u32, u64) = (<$ty>::BITS, $bias);

            #[allow(
                clippy::cast_possible_truncation,
                clippy::cast_sign_loss,
                reason = "the caller has checked the bits this truncates away"
            )]
            fn narrow(value: i64) -> Self {
                value as Self
            }
        }
    )*};
}

narrows! {
    i8 => 1 << 7,
    u8 => 0,
    i16 => 1 << 15,
    u16 => 0,
    i32 => 1 << 31,
    u32 => 0,
}

/// Narrows a page's values, refusing the page if any of them does not fit.
///
/// The check first and the conversion second, rather than a fallible conversion a value at a time.
/// Both loops here are ones a compiler widens: [`residue`] is three instructions a lane and a
/// narrowing store is one. The version before this was a `TryFrom` and a `collect` into a `Result`,
/// which is a compare, a branch and a short circuit a value at a time, and on ClickBench 39 it was
/// seven percent of the query. The version after that kept a running minimum and maximum, which is
/// the obvious way to ask and needs a 64 bit integer minimum that AVX2 does not have, so it stayed
/// a value at a time and was still ten percent of the same query.
///
/// An empty page has nothing to refuse, which falls out of the accumulator starting at zero rather
/// than needing a case of its own.
fn fit<T: Narrow>(values: &[i64]) -> Result<Vec<T>> {
    let mut spilled = 0u64;
    for value in values {
        spilled |= residue::<T>(*value);
    }
    if spilled != 0 {
        return Err(invalid("page value is not of its type"));
    }
    Ok(values.iter().map(|value| T::narrow(*value)).collect())
}

/// The same values back in the width the column is declared at.
///
/// A value that does not fit is a page that disagrees with the directory about what the column is,
/// which is a damaged file rather than a caller error, so it is refused rather than truncated.
fn narrowed(ty: &LogicalType, values: Vec<i64>) -> Result<Data> {
    Ok(match ty {
        LogicalType::TinyInt => Data::Int8(fit::<i8>(&values)?.into()),
        LogicalType::UTinyInt => Data::UInt8(fit::<u8>(&values)?.into()),
        LogicalType::SmallInt => Data::Int16(fit::<i16>(&values)?.into()),
        LogicalType::USmallInt => Data::UInt16(fit::<u16>(&values)?.into()),
        LogicalType::Integer | LogicalType::Date => Data::Int32(fit::<i32>(&values)?.into()),
        LogicalType::UInteger => Data::UInt32(fit::<u32>(&values)?.into()),
        LogicalType::BigInt
        | LogicalType::Timestamp
        | LogicalType::Time
        | LogicalType::TimeTz
        | LogicalType::TimestampTz
        | LogicalType::TimestampS
        | LogicalType::TimestampMs
        | LogicalType::TimestampNs => Data::Int64(values.into()),
        // A decimal is an integer of unscaled units, so the cascade reads back into whichever
        // integer the declared width says the column is stored as.
        LogicalType::Decimal { .. } => match ty.physical() {
            PhysicalType::Int16 => Data::Int16(fit::<i16>(&values)?.into()),
            PhysicalType::Int32 => Data::Int32(fit::<i32>(&values)?.into()),
            PhysicalType::Int64 => Data::Int64(values.into()),
            _ => return Err(invalid("cascade codec belongs to a decimal that is not an integer")),
        },
        _ => return Err(invalid("cascade codec belongs to a page that is not integers")),
    })
}

/// How many bytes a part of this type costs written out plainly, which is what the cascade has to
/// beat before it is worth the decode.
fn plain_width(ty: &LogicalType) -> Option<usize> {
    Some(match ty {
        LogicalType::TinyInt | LogicalType::UTinyInt => 1,
        LogicalType::SmallInt | LogicalType::USmallInt => 2,
        LogicalType::Integer | LogicalType::UInteger | LogicalType::Date => 4,
        LogicalType::BigInt
        | LogicalType::Timestamp
        | LogicalType::Time
        | LogicalType::TimeTz
        | LogicalType::TimestampTz
        | LogicalType::TimestampS
        | LogicalType::TimestampMs
        | LogicalType::TimestampNs => 8,
        LogicalType::Decimal { .. } => match ty.physical() {
            PhysicalType::Int16 => 2,
            PhysicalType::Int32 => 4,
            PhysicalType::Int64 => 8,
            // The widest decimals are stored as `i128`, which the cascade does not widen into, so
            // they take the plain path and there is nothing here to compare against.
            _ => return None,
        },
        _ => return None,
    })
}

/// A part's plain integers through the cascade, or `None` when nothing it offers is worth it.
///
/// What it has to beat is whatever the page would otherwise have cost, which is the bit packed form
/// where there is one and the plain width where there is not. Both are cheaper to decode than a
/// cascade, so a tie goes to them.
fn cascaded(
    flat: &Vector,
    ty: &LogicalType,
    packed: Option<&Packed<'_>>,
    settling: &mut Settling,
) -> Result<Option<Vec<u8>>> {
    let (Some(width), Some(data)) = (plain_width(ty), flat.data()) else { return Ok(None) };
    let Some(values) = widened(data) else { return Ok(None) };
    let plain = values.len().saturating_mul(width);
    let best = match packed {
        // The tag, the base, the word count and the words, which is what the codec 2 branch writes.
        Some(packed) => plain.min(21 + size_of_val(packed.words())),
        None => plain,
    };
    let out = settling.encode(&values)?;
    Ok((out.len() < best).then_some(out))
}

/// How often the parts of one column in one stripe search the cascade again, in parts.
///
/// A stripe is 64 parts, so this is four searches a stripe where there were 64. The search is
/// what the cascade costs: on ClickBench `hits` the integer cascade was about a tenth of the load's
/// CPU and nearly all of it under `encode_pages`, trying six trees on every part to keep the one
/// the part before had kept.
const SEARCH_EVERY: usize = 16;

/// What the parts of one column in one stripe have settled on in the integer cascade.
///
/// One of these per column per stripe, used in part order, so what a part comes out as depends on
/// the stripe and not on which thread wrote it or on how many there were.
#[derive(Debug, Default)]
struct Settling {
    /// The shape of the last part that was searched, with what its top level offered, its length
    /// and its row count, which is the size a replay is held to.
    shape: Option<Shape>,
    /// Parts replayed since that search.
    since: usize,
}

impl Settling {
    /// A part's integers through the cascade, replaying the settled shape where there is one.
    ///
    /// The replay is kept when it held and came out no more than a quarter bigger a row than the
    /// part the shape was searched on. Past that the column has changed under it and the part is
    /// searched. A replay that stopped fitting partway has already searched from where it stopped,
    /// so its shape is taken as the new one rather than searched a second time.
    fn encode(&mut self, values: &[i64]) -> Result<Vec<u8>> {
        if let Some(shape) = self.shape.as_ref().filter(|_| self.since < SEARCH_EVERY) {
            let replay = chooser::Replay::new(&shape.kinds, &Fixed).expecting(&shape.offered);
            let out = integer::encode_with(values, &replay)?;
            if !replay.held() {
                self.settle(&out, values.len(), replay.first_offered())?;
                return Ok(out);
            }
            let grown = (out.len() as u128) * (shape.rows as u128) * 4;
            if grown <= (shape.len as u128) * (values.len() as u128) * 5 {
                self.since += 1;
                return Ok(out);
            }
        }
        // A replay of nothing is the search, and says what the top level offered on the way.
        let search = chooser::Replay::new(&[], &Fixed);
        let out = integer::encode_with(values, &search)?;
        self.settle(&out, values.len(), search.first_offered())?;
        Ok(out)
    }

    fn settle(&mut self, out: &[u8], rows: usize, offered: Vec<integer::Kind>) -> Result<()> {
        let kinds = integer::shape(out)?;
        self.shape = Some(Shape { kinds, offered, len: out.len().max(1), rows: rows.max(1) });
        self.since = 0;
        Ok(())
    }
}

/// A searched part's cascade, what its top level was offered, and what it came to.
#[derive(Debug)]
struct Shape {
    kinds: Vec<integer::Kind>,
    offered: Vec<integer::Kind>,
    len: usize,
    rows: usize,
}

/// A part's dictionary codes through the integer cascade, or `None` when the cascade did not pay.
///
/// Until now this stream was a `u32` a row with nothing asked of it, and on ClickBench that was
/// 400,185,326 bytes for every one of the 28 varchar columns, the same count for `URL` as for a
/// column holding the empty string in nearly every row. Codes are dense integers counted from zero
/// and a part holds 1024 of them, which is the shape frame of reference is best at, and a column
/// with one value everywhere comes back a constant costing nothing per row rather than four bytes.
///
/// The result is taken only when it is smaller than the plain form. A cascade is allowed to come
/// out larger on a part whose codes are genuinely wide, `URL` has about sixty million distinct
/// values, and there is no reason to pay for the decode when it does.
/// A varchar page as one FSST layer, or `None` when it did not pay.
///
/// Until now a varchar page that neither the global dictionary nor the per page dictionary claimed
/// was written out raw: four bytes of offset a row and then the bytes. That is the right answer for
/// a page of values with nothing in common and the wrong one for a page of English, and a column of
/// comments is the case this exists for.
///
/// One layer and not the full string cascade, which is what the payload blocks of a global
/// dictionary go through. The cascade is a search: it encodes the page under every candidate it has
/// and recurses into the integer cascade for the lengths of each one, and on TPC-H `orders` that
/// took the write from 6.9 s to 48.3 s. It reads back no faster than the dictionary it replaced
/// either, 1.807 G instructions against 1.810 G for `select o_comment from orders`, because
/// unpicking a nest of layers a value at a time costs what the dictionary's payload block decode
/// cost. Raw pages of the same column read in 0.686 G, which says the whole of the difference is
/// what the page has to be put back together from.
///
/// FSST alone keeps most of what the cascade found and gives all of that back. Decoding it is one
/// pass over the payload into one buffer, the values are laid end to end in it the way the raw form
/// already lays them out, and what the reader hands a chunk is views over that buffer.
///
/// The page dictionary gets first refusal because it is cheaper still, and it wins on a page whose
/// values repeat. What is left for this is the page whose values mostly do not, which is exactly the
/// page that was being written raw.
///
/// Taken only when it comes out smaller than the raw form, so a page of incompressible values pays
/// nothing at read time for having been offered.
fn text_compressed(flat: &Vector) -> Result<Option<Vec<u8>>> {
    let mut values: Vec<&[u8]> = Vec::with_capacity(flat.len());
    let mut payload = 0_usize;
    for row in 0..flat.len() {
        // bytes_at: the rows were checked for UTF-8 on the way in, and checking them again here
        // was most of what the loop cost.
        let text = flat.bytes_at(row).unwrap_or(b"");
        payload = payload.saturating_add(text.len());
        values.push(text);
    }
    // What codec 0 writes for a varchar page: an offset a row and one more, then the payload.
    let plain = (flat.len() + 1).saturating_mul(4).saturating_add(payload);
    let Some(out) = string::encode_only(string::Kind::Fsst, &values)? else {
        return Ok(None);
    };
    Ok((out.len() < plain).then_some(out))
}

fn encoded_codes(codes: &[u32]) -> Result<Option<Vec<u8>>> {
    let wide: Vec<i64> = codes.iter().map(|code| i64::from(*code)).collect();
    let coded = integer::encode_with(&wide, &Codes)?;
    let plain = codes.len().saturating_mul(size_of::<u32>());
    Ok((coded.len() < plain).then_some(coded))
}

/// The validity of a page, which is a flag and then, when some rows are null and some are not, a
/// bit a row with the valid ones set.
fn push_validity(out: &mut Vec<u8>, flat: &Vector) {
    let flag = match flat.validity() {
        Validity::AllValid => 0,
        Validity::AllInvalid => 1,
        Validity::Mask(_) => 2,
    };
    out.push(flag);
    if flag == 2 {
        for group in (0..flat.len()).step_by(8) {
            let mut bits = 0_u8;
            for bit in 0..8 {
                if group + bit < flat.len() && !flat.is_null_at(group + bit) {
                    bits |= 1 << bit;
                }
            }
            out.push(bits);
        }
    }
}

/// One part of a column coded against its global dictionary as a page, from the codes and the
/// validity [`push_validity`] wrote for it.
///
/// The codes go through the integer cascade when that comes out smaller than four bytes a code,
/// which on a column that repeats itself it nearly always does, and are written as they are when it
/// does not.
fn coded_page(codes: &[u32], validity: &[u8]) -> Result<Vec<u8>> {
    let coded = encoded_codes(codes)?;
    let mut out = Vec::with_capacity(
        1 + validity.len() + coded.as_ref().map_or(size_of_val(codes), Vec::len),
    );
    out.push(if coded.is_some() { 4 } else { 3 });
    out.extend_from_slice(validity);
    match coded {
        Some(coded) => out.extend_from_slice(&coded),
        None => {
            for &code in codes {
                put_u32(&mut out, code);
            }
        }
    }
    Ok(out)
}

/// One part of one column as a page, for every column that is not coded against a global
/// dictionary. Those are built by [`coded_page`] from codes [`prepare`] handed out.
fn encode(vector: &Vector, settling: &mut Settling) -> Result<Vec<u8>> {
    let ty = vector.logical_type();
    // flatten: the file writer needs a uniform scalar page and does it once per loaded chunk.
    let flat = vector.flatten()?;
    let mut out = Vec::new();
    let dictionary = if coded_type(ty) { string_dictionary(&flat)? } else { None };
    let compressed_text =
        if dictionary.is_none() && coded_type(ty) { text_compressed(&flat)? } else { None };
    let packed_vector = if dictionary.is_none() { Some(flat.bit_packed()?) } else { None };
    let packed = packed_vector.as_ref().and_then(Vector::packed_parts);
    // Only where nothing else has claimed the page, which is the plain integer case. A packed part
    // is still on the table because the cascade has to beat it too: the bit pack takes a part only
    // when it halves it, so a column that shrinks by a third was coming out whole.
    let cascade =
        if dictionary.is_none() { cascaded(&flat, ty, packed.as_ref(), settling)? } else { None };
    out.push(if cascade.is_some() {
        5
    } else if dictionary.is_some() {
        1
    } else if compressed_text.is_some() {
        6
    } else if packed.is_some() {
        2
    } else {
        0
    });
    push_validity(&mut out, &flat);
    if let Some(cascade) = cascade {
        out.extend_from_slice(&cascade);
        return Ok(out);
    }
    if let Some(dictionary) = dictionary {
        out.extend_from_slice(&dictionary);
        return Ok(out);
    }
    if let Some(compressed_text) = compressed_text {
        out.extend_from_slice(&compressed_text);
        return Ok(out);
    }
    if let Some(packed) = packed {
        if packed.offset() != 0 {
            return Err(invalid("writer received a sliced packed vector"));
        }
        out.push(u8::try_from(packed.width()).map_err(|_| invalid("packed width overflow"))?);
        out.extend_from_slice(&packed.base().to_le_bytes());
        put_u32(
            &mut out,
            u32::try_from(packed.words().len()).map_err(|_| invalid("too many packed words"))?,
        );
        for word in packed.words() {
            put_u64(&mut out, *word);
        }
        return Ok(out);
    }
    let data = flat.data().ok_or_else(|| invalid("scalar column did not flatten"))?;
    match (ty, data) {
        (LogicalType::TinyInt, Data::Int8(values)) => {
            for value in &**values {
                out.extend_from_slice(&value.to_le_bytes());
            }
        }
        (LogicalType::UTinyInt, Data::UInt8(values)) => {
            for value in &**values {
                out.extend_from_slice(&value.to_le_bytes());
            }
        }
        (LogicalType::SmallInt, Data::Int16(values)) => {
            for value in &**values {
                out.extend_from_slice(&value.to_le_bytes());
            }
        }
        (LogicalType::USmallInt, Data::UInt16(values)) => {
            for value in &**values {
                out.extend_from_slice(&value.to_le_bytes());
            }
        }
        (LogicalType::UInteger, Data::UInt32(values)) => {
            for value in &**values {
                out.extend_from_slice(&value.to_le_bytes());
            }
        }
        (LogicalType::UBigInt, Data::UInt64(values)) => {
            for value in &**values {
                out.extend_from_slice(&value.to_le_bytes());
            }
        }
        (LogicalType::Integer | LogicalType::Date, Data::Int32(values)) => {
            for value in &**values {
                out.extend_from_slice(&value.to_le_bytes());
            }
        }
        (
            LogicalType::BigInt
            | LogicalType::Timestamp
            | LogicalType::Time
            | LogicalType::TimeTz
            | LogicalType::TimestampTz
            | LogicalType::TimestampS
            | LogicalType::TimestampMs
            | LogicalType::TimestampNs,
            Data::Int64(values),
        ) => {
            for value in &**values {
                out.extend_from_slice(&value.to_le_bytes());
            }
        }
        // A hugeint and a uuid are both the 128 bit lane, and a uuid's bits are the ones the rest of
        // the engine already carries it in, so nothing about the value changes on the way down.
        (LogicalType::HugeInt | LogicalType::Uuid, Data::Int128(values)) => {
            for value in &**values {
                out.extend_from_slice(&value.to_le_bytes());
            }
        }
        (LogicalType::UHugeInt, Data::UInt128(values)) => {
            for value in &**values {
                out.extend_from_slice(&value.to_le_bytes());
            }
        }
        // Plainly, in the IEEE bytes. The integer encodings do not apply to a float and none of the
        // float codecs is worth having before somebody has measured a corpus of them.
        (LogicalType::Float, Data::Float32(values)) => {
            for value in &**values {
                out.extend_from_slice(&value.to_le_bytes());
            }
        }
        (LogicalType::Double, Data::Float64(values)) => {
            for value in &**values {
                out.extend_from_slice(&value.to_le_bytes());
            }
        }
        // Three counts and not one number. Months, days and microseconds stay apart on disk because
        // they are apart in the value: a month is not a fixed number of days and a day is not a
        // fixed number of microseconds, which is the whole reason the type has three fields.
        (LogicalType::Interval, Data::Interval(values)) => {
            for (months, days, micros) in &**values {
                out.extend_from_slice(&months.to_le_bytes());
                out.extend_from_slice(&days.to_le_bytes());
                out.extend_from_slice(&micros.to_le_bytes());
            }
        }
        (LogicalType::Boolean, Data::Bool(values)) => {
            for value in &**values {
                out.push(u8::from(*value));
            }
        }
        // The unscaled integer and nothing else. Scale is a property of the column and it is in the
        // directory already, so writing it a value at a time would be paying for it twice.
        (LogicalType::Decimal { .. }, Data::Int16(values)) => {
            for value in &**values {
                out.extend_from_slice(&value.to_le_bytes());
            }
        }
        (LogicalType::Decimal { .. }, Data::Int32(values)) => {
            for value in &**values {
                out.extend_from_slice(&value.to_le_bytes());
            }
        }
        (LogicalType::Decimal { .. }, Data::Int64(values)) => {
            for value in &**values {
                out.extend_from_slice(&value.to_le_bytes());
            }
        }
        (LogicalType::Decimal { .. }, Data::Int128(values)) => {
            for value in &**values {
                out.extend_from_slice(&value.to_le_bytes());
            }
        }
        // A blob and a bit string go down the way a varchar does, because the layout is the same
        // one: an offset a value and then the bytes. What is not the same is that nothing here may
        // read the payload as text, which is why this arm asks the column for bytes rather than for
        // a string, and why the codecs above that do read text are all asked of a varchar by name.
        (LogicalType::Varchar | LogicalType::Blob | LogicalType::Bit, Data::Varlen(values)) => {
            let mut bytes = Vec::new();
            put_u32(&mut out, 0);
            for row in 0..vector.len() {
                let value = values.bytes(row).ok_or_else(|| invalid("string view is invalid"))?;
                bytes.extend_from_slice(value);
                put_u32(
                    &mut out,
                    u32::try_from(bytes.len())
                        .map_err(|_| invalid("string payload exceeds 4GiB"))?,
                );
            }
            out.extend_from_slice(&bytes);
        }
        _ => return Err(Error::not_implemented(format!("native page for {ty}"))),
    }
    Ok(out)
}

fn put_varint(out: &mut Vec<u8>, mut value: u32) {
    while value >= 0x80 {
        out.push((value as u8 & 0x7f) | 0x80);
        value >>= 7;
    }
    out.push(value as u8);
}

/// The distinct codes of one part, which is what a stripe's membership index is merged from.
fn unique_codes(codes: &[u32]) -> Vec<u32> {
    let mut unique = codes.to_vec();
    unique.sort_unstable();
    unique.dedup();
    unique
}

/// The union of the sorted distinct codes of every part in a stripe.
///
/// Pairwise up a tree rather than one long list concatenated and sorted. Both are the same order of
/// work on paper and the tree is the one that does not sort what is already in order: sixty four
/// sorted lists become one in six passes over the values.
fn merged_codes(lists: Vec<Vec<u32>>) -> Vec<u32> {
    let mut lists = lists;
    while lists.len() > 1 {
        let mut next = Vec::with_capacity(lists.len().div_ceil(2));
        for pair in lists.chunks(2) {
            match pair {
                [left, right] => next.push(merged_pair(left, right)),
                [only] => next.push(only.clone()),
                _ => {}
            }
        }
        lists = next;
    }
    lists.pop().unwrap_or_default()
}

fn merged_pair(left: &[u32], right: &[u32]) -> Vec<u32> {
    let mut out = Vec::with_capacity(left.len().saturating_add(right.len()));
    let mut at = 0;
    let mut to = 0;
    while at < left.len() && to < right.len() {
        match left[at].cmp(&right[to]) {
            Ordering::Less => {
                out.push(left[at]);
                at += 1;
            }
            Ordering::Greater => {
                out.push(right[to]);
                to += 1;
            }
            Ordering::Equal => {
                out.push(left[at]);
                at += 1;
                to += 1;
            }
        }
    }
    out.extend_from_slice(&left[at..]);
    out.extend_from_slice(&right[to..]);
    out
}

/// The widest bounds and the total null count of a stripe, from the bounds of its parts.
///
/// A bound that is missing from any part is missing from the stripe, because a missing bound means
/// nothing is known and a stripe that holds an unknown cannot claim one.
fn merged_range(ranges: impl Iterator<Item = Range>) -> Range {
    let mut merged = Range::default();
    let mut first = true;
    for range in ranges {
        merged.nulls = merged.nulls.saturating_add(range.nulls);
        // Both of these have to survive every part, so one part that could not say anything makes
        // the stripe unable to say it either. A sum is dropped on overflow rather than wrapped,
        // which leaves the stripe with exact ends and no total, which is a true thing to say.
        merged.sum = match (merged.sum.take(), range.sum) {
            (Some(held), Some(next)) if !first => held.checked_add(next),
            (_, next) if first => next,
            _ => None,
        };
        merged.exact = if first { range.exact } else { merged.exact && range.exact };
        if first {
            merged.low = range.low;
            merged.high = range.high;
            first = false;
            continue;
        }
        merged.low = match (merged.low.take(), range.low) {
            (Some(held), Some(next)) => Some(held.smaller(next)),
            _ => None,
        };
        merged.high = match (merged.high.take(), range.high) {
            (Some(held), Some(next)) => Some(held.larger(next)),
            _ => None,
        };
    }
    merged
}

/// One stripe's sieves for one column: the part count, a length for each part, then their bytes.
///
/// One page for the whole stripe rather than one per part, because a part's sieve is a few hundred
/// bytes and sixty four of those are sixty four directory entries and sixty four reads for something
/// a scan walks straight through. A part with no sieve writes a length of zero and costs four bytes.
/// `bound` cut down to [`PART_BOUND_BYTES`], still a bound of the side it was.
///
/// A prefix of a string sorts at or before the string, so cutting one down leaves a low end that is
/// still a low end. A high end has to go the other way, so the cut prefix is stepped up at the last
/// byte that can carry it, and a prefix of nothing but `0xFF` has no such byte and gives up the
/// bound rather than claiming one that is too small. Anything that is not a string is already a
/// fixed width and is left alone.
fn shortened(bound: Option<Bound>, high: bool) -> Option<Bound> {
    match bound {
        Some(Bound::Bytes(mut value)) if value.len() > PART_BOUND_BYTES => {
            value.truncate(PART_BOUND_BYTES);
            if !high {
                return Some(Bound::Bytes(value));
            }
            while let Some(last) = value.pop() {
                if last < u8::MAX {
                    value.push(last + 1);
                    return Some(Bound::Bytes(value));
                }
            }
            None
        }
        other => other,
    }
}

/// The ranges of one column's parts of one stripe, as a page.
///
/// The two ends and the null count, and not `exact` or the total. Those two answer a `MIN` or a
/// `SUM` out of the directory, and the directory already answers those per stripe, where the same
/// number costs sixty times less to keep. What a part range is for is skipping the part, and
/// skipping needs the ends. So a range read back from here says it is not exact, which is true of a
/// string end that was cut down anyway.
fn encode_part_ranges(ranges: &[Range]) -> Result<Vec<u8>> {
    let mut out = Vec::new();
    put_u32(
        &mut out,
        u32::try_from(ranges.len()).map_err(|_| invalid("too many parts in a stripe"))?,
    );
    for range in ranges {
        put_bound(&mut out, shortened(range.low.clone(), false).as_ref())?;
        put_bound(&mut out, shortened(range.high.clone(), true).as_ref())?;
        put_u32(&mut out, u32::try_from(range.nulls).map_err(|_| invalid("null count overflow"))?);
    }
    Ok(out)
}

/// The ranges one encoded page holds, one entry per part of the stripe.
fn decode_part_ranges(bytes: &[u8]) -> Result<Vec<Range>> {
    let mut cur = Cursor::new(bytes);
    let parts = cur.u32()? as usize;
    let mut out = Vec::new();
    for _ in 0..parts {
        let low = cur.bound()?;
        let high = cur.bound()?;
        let nulls = cur.u32()? as usize;
        out.push(Range { low, high, nulls, exact: false, sum: None });
    }
    Ok(out)
}

fn encode_sieves<'a>(sieves: impl Iterator<Item = &'a Option<Sieve>>) -> Result<Vec<u8>> {
    let held: Vec<&Option<Sieve>> = sieves.collect();
    let mut out = Vec::new();
    put_u32(
        &mut out,
        u32::try_from(held.len()).map_err(|_| invalid("too many parts in a stripe"))?,
    );
    for sieve in &held {
        let length = sieve.as_ref().map_or(0, Sieve::len);
        put_u32(&mut out, u32::try_from(length).map_err(|_| invalid("sieve length overflow"))?);
    }
    // flatten: a part with no sieve wrote a length of zero above and contributes no bytes here.
    for sieve in held.into_iter().flatten() {
        out.extend_from_slice(&sieve.to_bytes());
    }
    Ok(out)
}

/// The sieves one encoded page holds, one entry per part of the stripe.
///
/// A part whose bytes are not a sieve this version understands comes back as `None`, which is a part
/// that gets read. That is how a file written by a later version of the sieve stays readable rather
/// than being a corrupt page.
fn decode_sieves(bytes: &[u8]) -> Result<Vec<Option<Sieve>>> {
    let parts = u32::from_le_bytes(
        bytes
            .get(..4)
            .ok_or_else(|| invalid("sieve page is truncated"))?
            .try_into()
            .map_err(|_| invalid("sieve page is truncated"))?,
    ) as usize;
    let mut lengths = Vec::with_capacity(parts);
    for part in 0..parts {
        let at = 4 + part * 4;
        let field = bytes.get(at..at + 4).ok_or_else(|| invalid("sieve page is truncated"))?;
        lengths.push(u32::from_le_bytes(
            field.try_into().map_err(|_| invalid("sieve page is truncated"))?,
        ) as usize);
    }
    let mut at = 4 + parts * 4;
    let mut out = Vec::with_capacity(parts);
    for length in lengths {
        if length == 0 {
            out.push(None);
            continue;
        }
        let end = at.checked_add(length).ok_or_else(|| invalid("sieve page is truncated"))?;
        let field = bytes.get(at..end).ok_or_else(|| invalid("sieve page is truncated"))?;
        out.push(Sieve::from_bytes(field));
        at = end;
    }
    if at != bytes.len() {
        return Err(invalid("sieve page has trailing bytes"));
    }
    Ok(out)
}

/// One stripe's membership index: the code count and then the codes as ascending deltas.
///
/// The codes have to be sorted and distinct already, which is what [`unique_codes`] and
/// [`merged_codes`] hand over. Anything else decodes as different codes, so neither of those two is
/// a step a caller can skip.
fn encode_membership(unique: &[u32]) -> Vec<u8> {
    let mut out = Vec::with_capacity(unique.len().saturating_mul(2).saturating_add(5));
    put_varint(&mut out, u32::try_from(unique.len()).unwrap_or(u32::MAX));
    let mut previous = 0;
    for (at, &code) in unique.iter().enumerate() {
        put_varint(&mut out, if at == 0 { code } else { code - previous });
        previous = code;
    }
    out
}

fn take_varint(bytes: &[u8], at: &mut usize) -> Result<u32> {
    let mut value = 0_u32;
    for shift in (0..35).step_by(7) {
        let byte = *bytes.get(*at).ok_or_else(|| invalid("membership varint is truncated"))?;
        *at += 1;
        let part = u32::from(byte & 0x7f);
        if shift == 28 && part > 0x0f {
            return Err(invalid("membership varint overflow"));
        }
        value = value
            .checked_add(
                part.checked_shl(shift).ok_or_else(|| invalid("membership varint overflow"))?,
            )
            .ok_or_else(|| invalid("membership varint overflow"))?;
        if byte & 0x80 == 0 {
            return Ok(value);
        }
    }
    Err(invalid("membership varint is too long"))
}

fn decode_membership(bytes: &[u8]) -> Result<Vec<u32>> {
    let mut at = 0;
    let count = take_varint(bytes, &mut at)? as usize;
    let mut codes = Vec::with_capacity(count);
    let mut previous = 0_u32;
    for index in 0..count {
        let delta = take_varint(bytes, &mut at)?;
        let code = if index == 0 {
            delta
        } else {
            previous.checked_add(delta).ok_or_else(|| invalid("membership code overflow"))?
        };
        if index > 0 && code <= previous {
            return Err(invalid("membership codes are not increasing"));
        }
        codes.push(code);
        previous = code;
    }
    if at != bytes.len() {
        return Err(invalid("membership page has trailing bytes"));
    }
    Ok(codes)
}

fn string_dictionary(vector: &Vector) -> Result<Option<Vec<u8>>> {
    let mut by_text = HashMap::new();
    let mut values = Vec::new();
    let mut codes = Vec::with_capacity(vector.len());
    let mut plain_bytes = 0_usize;
    for row in 0..vector.len() {
        let text = vector.bytes_at(row).unwrap_or(b"");
        plain_bytes = plain_bytes.saturating_add(text.len());
        let code = match by_text.get(text) {
            Some(&code) => code,
            None => {
                let code = u32::try_from(values.len())
                    .map_err(|_| invalid("too many dictionary values"))?;
                by_text.insert(text, code);
                values.push(text);
                code
            }
        };
        codes.push(code);
    }
    let dictionary_bytes = values.iter().map(|value| value.len()).sum::<usize>();
    let encoded = 8_usize
        .saturating_add((values.len() + 1).saturating_mul(4))
        .saturating_add(dictionary_bytes)
        .saturating_add(codes.len().saturating_mul(4));
    let plain = (vector.len() + 1).saturating_mul(4).saturating_add(plain_bytes);
    if encoded >= plain {
        return Ok(None);
    }
    let mut out = Vec::with_capacity(encoded);
    put_u32(
        &mut out,
        u32::try_from(values.len()).map_err(|_| invalid("too many dictionary values"))?,
    );
    put_u32(
        &mut out,
        u32::try_from(dictionary_bytes).map_err(|_| invalid("dictionary payload exceeds 4GiB"))?,
    );
    let mut offset = 0_u32;
    put_u32(&mut out, offset);
    for value in &values {
        offset = offset
            .checked_add(
                u32::try_from(value.len()).map_err(|_| invalid("dictionary value is too long"))?,
            )
            .ok_or_else(|| invalid("dictionary payload exceeds 4GiB"))?;
        put_u32(&mut out, offset);
    }
    for value in values {
        out.extend_from_slice(value);
    }
    for code in codes {
        put_u32(&mut out, code);
    }
    Ok(Some(out))
}

/// The room one closing column takes under [`CLOSE_BYTES`], given back when dropped.
struct Room<'a, T> {
    state: &'a Mutex<(T, usize)>,
    finished: &'a Condvar,
    bytes: usize,
}

impl<T> Drop for Room<'_, T> {
    fn drop(&mut self) {
        let mut held = self.state.lock().unwrap_or_else(PoisonError::into_inner);
        held.1 -= self.bytes;
        drop(held);
        self.finished.notify_all();
    }
}

/// One column's work at the end of a load, as [`Writer::close_columns`] schedules it.
enum Closing<'a> {
    /// A numeric column's frequencies, and whether to count its distinct values exactly.
    Numeric {
        column: usize,
        counted: bool,
    },
    Dictionary {
        index: usize,
        dictionary: &'a GlobalDictionary,
    },
}

/// What one [`Closing`] came back with, by column.
enum Closed {
    Numeric(usize, (Option<FrequencySummary>, Option<u64>)),
    Dictionary(usize, ClosedDictionary),
}

/// What [`Writer::close_dictionary`] builds for one column and [`Writer::close`] writes.
struct ClosedDictionary {
    /// `None` for a demoted dictionary, which holds only some of the column. See [`DEMOTED`].
    distinct: Option<u64>,
    frequencies: Option<FrequencySummary>,
    texts: Vec<Option<Vec<u8>>>,
    hosts: Option<host::HostSummary>,
    encoded: EncodedDictionary,
    /// The bytes of the column's payload blocks, which are already in the file.
    payload: u64,
}

struct EncodedDictionary {
    index: Vec<u8>,
    ranks: Vec<u8>,
    grams: Vec<u8>,
}

/// Sorts codes into the byte order of the values they name, eight bytes of depth at a time.
///
/// # What the shape of the data does to a comparison sort
///
/// Distinct values against distinct prefixes, on the eight million row `hits`:
///
/// ```text
///   distinct   first 8   first 16   first 32   column
///  2,266,417        50      8,892    232,630   URL
///  2,346,025        49      8,534    204,060   Referer
///  1,357,764    81,362    348,340    861,579   Title
/// ```
///
/// Two and a quarter million URLs have fifty distinct first eight bytes between them, because they
/// all begin `http://` and then a host and there are not many hosts. So a sort that leads with
/// those eight bytes settles almost nothing on `URL` and `Referer`, whatever the comment on it used
/// to say, and almost every pair falls through to a comparison of whole values that agree for most
/// of their length. `Title` is free text and separates at eight bytes, which is why the design
/// looked right when it was written.
///
/// # What is done about it
///
/// Sort on eight bytes of the value at the current depth, held beside the code, and then take each
/// run that those eight bytes leave tied and sort it again on the next eight. A value is fetched
/// from the payload once per eight bytes of depth rather than once per comparison, and the sort
/// itself runs over an array of integers that is in cache rather than over pointers into a payload
/// that is hundreds of megabytes.
///
/// That is the whole trick, and it matters because the payload touch is the expensive part. The
/// bytes themselves are nearly free once the line is in cache, so reading eight at a time and
/// throwing away the ones that were not needed beats going back for each one.
///
/// # Why the length has to be carried
///
/// The eight bytes are padded with zero when the value has fewer than eight left, and a zero byte
/// can appear in a value, so equal keys do not mean equal bytes. What is true is that a value which
/// ran out inside the window is a prefix of any other value with the same key, and a prefix sorts
/// first, so how many of the eight bytes were real is the tie break and nothing further is needed.
/// A run is only worth another pass when all eight were real, because otherwise the run is one
/// value: a dictionary holds a value once.
fn sort_by_value<'a>(codes: &mut [u32], values: impl Fn(u32) -> &'a [u8]) {
    let mut work = vec![(0, codes.len(), 0)];
    let mut keyed: Vec<(u64, u8, u32)> = Vec::new();
    while let Some((from, to, depth)) = work.pop() {
        let part = &mut codes[from..to];
        keyed.clear();
        keyed.extend(part.iter().map(|&code| {
            let value = values(code);
            let rest = value.get(depth..).unwrap_or_default();
            (head(rest), rest.len().min(8) as u8, code)
        }));
        keyed.sort_unstable();
        for (slot, entry) in part.iter_mut().zip(keyed.iter()) {
            *slot = entry.2;
        }
        let mut start = 0;
        while start < keyed.len() {
            let (key, taken, _) = keyed[start];
            let mut end = start + 1;
            while end < keyed.len() && keyed[end].0 == key && keyed[end].1 == taken {
                end += 1;
            }
            if taken == 8 && end - start > 1 {
                work.push((from + start, from + end, depth + 8));
            }
            start = end;
        }
    }
}

/// How few codes are worth sorting on more than one thread.
const PARALLEL_SORT_MIN: usize = 1 << 16;

/// How many buckets a thread gets in [`sort_by_value_across`], so that a thread that drew a slow
/// bucket is not what the others wait for.
const BUCKETS_PER_WORKER: usize = 4;

/// How many sampled codes stand for each bucket when the splitters are picked.
const SAMPLES_PER_BUCKET: usize = 32;

/// [`sort_by_value`] over `workers` threads, with the same answer.
///
/// A sample sort. A sample of the codes is sorted and cut into as many equal runs as there are
/// buckets, and the values at the cuts are the splitters. Every code goes to the bucket its value
/// falls in by a binary search of the splitters, the buckets are laid end to end in splitter order,
/// and each bucket is then sorted on its own by whichever thread takes it. Every value in a bucket
/// sorts after every value in the bucket before, so the buckets sorted one by one are the codes
/// sorted.
///
/// The answer is the one [`sort_by_value`] gives down to the order of equal values, not only the
/// order of different ones. A global dictionary holds each value once, so there are none, but the
/// sort does not rely on it: equal values land in the same bucket in code order, which is the order
/// [`sort_by_value`] leaves them in, since the code is the last thing it sorts on.
///
/// On the 10m ClickBench sample the close sorts five columns of one to three and a half million
/// distinct values, one column at a time, and until this each sort ran on one thread while the
/// other thirty one waited for it.
fn sort_by_value_across<'a>(
    codes: &mut [u32],
    values: impl Fn(u32) -> &'a [u8] + Sync,
    workers: usize,
) {
    if workers <= 1 || codes.len() < PARALLEL_SORT_MIN {
        sort_by_value(codes, values);
        return;
    }
    let buckets = workers * BUCKETS_PER_WORKER;
    let wanted = buckets * SAMPLES_PER_BUCKET;
    let mut sample = (0..wanted).map(|at| codes[at * codes.len() / wanted]).collect::<Vec<_>>();
    sort_by_value(&mut sample, &values);
    let splitters =
        (1..buckets).map(|cut| values(sample[cut * sample.len() / buckets])).collect::<Vec<_>>();
    let values = &values;
    let splitters = &splitters;
    let per = codes.len().div_ceil(workers);
    // Which bucket each code goes to, a run of the codes per thread.
    let places = std::thread::scope(|scope| {
        codes
            .chunks(per)
            .map(|run| {
                scope.spawn(move || {
                    run.iter()
                        .map(|&code| {
                            let value = values(code);
                            splitters.partition_point(|splitter| *splitter <= value) as u32
                        })
                        .collect::<Vec<_>>()
                })
            })
            .collect::<Vec<_>>()
            .into_iter()
            .flat_map(|handle| {
                handle.join().unwrap_or_else(|panic| std::panic::resume_unwind(panic))
            })
            .collect::<Vec<_>>()
    });
    let mut starts = vec![0_usize; buckets + 1];
    for &place in &places {
        starts[place as usize + 1] += 1;
    }
    for bucket in 0..buckets {
        starts[bucket + 1] += starts[bucket];
    }
    let mut laid = vec![0_u32; codes.len()];
    let mut next = starts.clone();
    for (&code, &place) in codes.iter().zip(&places) {
        laid[next[place as usize]] = code;
        next[place as usize] += 1;
    }
    drop(places);
    let mut runs = Vec::with_capacity(buckets);
    let mut rest = laid.as_mut_slice();
    for bucket in 0..buckets {
        let (run, after) = rest.split_at_mut(starts[bucket + 1] - starts[bucket]);
        runs.push(run);
        rest = after;
    }
    // The largest buckets first, since they are taken from the back.
    runs.sort_by_key(|run| run.len());
    let queue = Mutex::new(runs);
    std::thread::scope(|scope| {
        for _ in 0..workers {
            scope.spawn(|| {
                loop {
                    let taken = queue.lock().unwrap_or_else(PoisonError::into_inner).pop();
                    let Some(run) = taken else { break };
                    sort_by_value(run, values);
                }
            });
        }
    });
    codes.copy_from_slice(&laid);
}

/// The first eight bytes of a value as an integer that sorts the way the bytes sort.
fn head(bytes: &[u8]) -> u64 {
    let mut word = [0; 8];
    let take = bytes.len().min(8);
    word[..take].copy_from_slice(&bytes[..take]);
    u64::from_be_bytes(word)
}

/// One column's dictionary page, which is its index and its sorted order.
///
/// The payload is not in it. Its blocks are in the file already, written as each was encoded, and
/// `places` says where, in block order. With `scattered` set the index records each block's start
/// and length, so a reader can find one wherever it went.
///
/// `scattered` false lays the blocks out the way a file written before format 26 has them, one
/// behind the next with only the ends recorded. Nothing in the writer asks for that any more. It is
/// kept because [`open_global_dictionary`] still reads those files and a reading path that nothing
/// can produce is a reading path nothing tests.
fn encode_global_dictionary(
    dictionary: &GlobalDictionary,
    order: &[(u64, u32)],
    places: &[Placed],
    scattered: bool,
) -> Result<EncodedDictionary> {
    let values = dictionary.values();
    if order.len() != values {
        return Err(invalid("global dictionary order does not cover its values"));
    }
    let blocks = values.div_ceil(TEXT_PAYLOAD_VALUES);
    if places.len() != blocks {
        return Err(invalid("global dictionary payload is not the blocks it says it is"));
    }
    if dictionary.grams.len() != blocks {
        return Err(invalid("global dictionary signatures do not cover its blocks"));
    }
    let (ranks, rank_ends) = encode_ranks(order, code_width(values))?;
    let rank_blocks = values.div_ceil(TEXT_RANK_BLOCK);
    let offset_bits = offset_width(&dictionary.ends);
    let payload_words = if scattered { 3 } else { 2 };
    let index_len = DICTIONARY_HEADER
        .checked_add(offset_bytes(values, offset_bits))
        .and_then(|len| len.checked_add(blocks.checked_mul(payload_words * 8)?))
        .and_then(|len| len.checked_add(rank_blocks.checked_mul(16)?))
        .and_then(|len| len.checked_add(8))
        .ok_or_else(|| invalid("global dictionary index length overflow"))?;
    let mut index = Vec::with_capacity(index_len);
    put_u32(
        &mut index,
        u32::try_from(values).map_err(|_| invalid("global dictionary has too many values"))?,
    );
    put_u32(&mut index, TEXT_PAYLOAD_VALUES as u32);
    put_u32(
        &mut index,
        u32::try_from(blocks).map_err(|_| invalid("global dictionary has too many blocks"))?,
    );
    let flag = (if scattered { DICTIONARY_SCATTERED } else { 0 })
        | DICTIONARY_GRAMS
        | DICTIONARY_WIDE_GRAMS;
    put_u32(&mut index, offset_bits as u32 | flag);
    encode_offsets(&dictionary.ends, offset_bits, &mut index)?;
    // Where each block is and how long it is, so a reader can find one. The stored blocks are
    // shorter than the decoded ones and by a different amount each, so their lengths are the one
    // thing the offsets above no longer say, and where they start is no longer arithmetic on the
    // block before once a block is written the moment it is encoded.
    let mut end = 0_u64;
    for place in places {
        if scattered {
            put_u64(&mut index, place.start);
            put_u64(&mut index, place.length);
        } else {
            end = end
                .checked_add(place.length)
                .ok_or_else(|| invalid("global dictionary payload overflow"))?;
            put_u64(&mut index, end);
        }
    }
    for place in places {
        put_u64(&mut index, place.hash);
    }
    // The same two lists for the sorted order. A rank block is packed at whatever width its own
    // heads need, so where one ends is no longer arithmetic on the block number.
    if rank_ends.len() != rank_blocks {
        return Err(invalid("global dictionary order is not the blocks it says it is"));
    }
    for end in &rank_ends {
        put_u64(&mut index, *end);
    }
    let mut at = 0_usize;
    for end in &rank_ends {
        let end = usize::try_from(*end).map_err(|_| invalid("global dictionary order overflow"))?;
        put_u64(&mut index, checksum(&ranks[at..end]));
        at = end;
    }
    let gram_len = blocks
        .checked_mul(TEXT_GRAM_BYTES)
        .ok_or_else(|| invalid("global dictionary signature count overflow"))?;
    let mut grams = Vec::with_capacity(gram_len);
    for block in &dictionary.grams {
        grams.extend_from_slice(block);
    }
    put_u64(&mut index, checksum(&grams));
    if index.len() != index_len {
        return Err(invalid("global dictionary index is not the length it was laid out for"));
    }
    Ok(EncodedDictionary { index, ranks, grams })
}

/// How many blocks of the payload the shape is settled on.
///
/// Eight blocks is 8,192 values, which is the sample `chooser::Sampled` draws and is that size for
/// the same reason. They are spread across the dictionary rather than taken off the front, because
/// a dictionary is in the order values were first seen and the front of it is the first morsel of
/// the load.
const PAYLOAD_SAMPLE_BLOCKS: usize = 8;

/// The shapes the payload encoder picks between.
///
/// Narrow on purpose. The exhaustive search encodes every candidate at every level and runs at two
/// to six megabytes a second on this data, which over the twelve gigabytes of dictionary `hits`
/// carries is about an hour of processor time, so it cannot be what a load does. Each of these
/// settles the outer level and the one below it, which is where almost all of that hour goes, and
/// leaves the levels under them to the exhaustive search where the chunks are small enough for it
/// to cost nothing.
///
/// Measured on the five ClickBench columns that have a dictionary worth the name, at 1,024 values a
/// block, against the exhaustive search over the same blocks:
///
/// | column | exhaustive | FRONT then LZ | LZ then FSST | LZ then PLAIN |
/// |---|---|---|---|---|
/// | 2 | 2.923 at 4.3 MB/s | 2.587 at 21.2 | 2.593 at 36.1 | 2.538 at 53.6 |
/// | 13 | 3.093 at 3.1 | 3.029 at 36.4 | 2.921 at 35.7 | 2.770 at 82.9 |
/// | 14 | 2.330 at 2.1 | 2.283 at 24.3 | 2.213 at 23.5 | 2.113 at 67.6 |
/// | 39 | 2.459 at 5.3 | 2.147 at 10.6 | 2.145 at 29.3 | 2.088 at 43.1 |
/// | 56 | 4.694 at 6.3 | 4.381 at 51.0 | 4.172 at 50.6 | 3.983 at 86.8 |
///
/// The best of the three per column is 98 percent of the exhaustive ratio for a tenth of the time.
/// `FSST` and `PLAIN` on their own are in the list as a floor rather than to win. `FSST` is the
/// right answer for text that does not share prefixes with its neighbours, and `PLAIN` is there so
/// that a column nothing compresses is found out in the sample and written at a gigabyte a second
/// rather than searched for an answer that does not exist.
fn payload_shapes() -> Vec<chooser::Settled> {
    let integers = vec![integer::Kind::Packed];
    [
        vec![string::Kind::Front, string::Kind::Lz],
        vec![string::Kind::Lz, string::Kind::Fsst],
        vec![string::Kind::Lz, string::Kind::Plain],
        vec![string::Kind::Fsst],
        vec![string::Kind::Plain],
    ]
    .into_iter()
    .map(|strings| chooser::Settled::new(strings, integers.clone()))
    .collect()
}

/// Syncs the file, and counts the sync and how long it took as a publish wait when a load is being
/// profiled.
///
/// A wait rather than time, because the time is already in the publish span around it. What the
/// wait columns add is how much of publish was the device, which on the WSL2 disk of the gaming PC
/// is most of it: a sync there costs about two milliseconds (see `rudb_device_card`).
fn synced(file: &dyn rudb_io::File, profile: Option<&LoadProfile>) -> Result<()> {
    let started = profile.map(|_| std::time::Instant::now());
    file.sync()?;
    if let (Some(profile), Some(started)) = (profile, started) {
        profile.waited(
            Stage::Publish,
            u64::try_from(started.elapsed().as_nanos()).unwrap_or(u64::MAX),
        );
    }
    Ok(())
}

/// One sealed dictionary block on its way to being encoded outside the writer's lock.
///
/// Handed out by the merge that sealed it and encoded with the pages of the same stripe. See
/// [`GlobalDictionary::hand_out`].
#[derive(Debug)]
pub(crate) struct Unencoded {
    column: usize,
    at: usize,
    ends: Vec<u32>,
    bytes: Vec<u8>,
    shape: chooser::Settled,
}

impl Unencoded {
    /// The encoded block and its signature.
    pub(crate) fn encode(&self) -> Result<EncodedBlock> {
        let values = block_values(&self.ends, &self.bytes);
        Ok((string::encode_with(&values, &self.shape)?, block_grams(&values)))
    }

    /// The column and the block number the encoded block goes back to.
    pub(crate) fn place(&self) -> (usize, usize) {
        (self.column, self.at)
    }
}

/// One encoded dictionary block and the signature of the values in it.
///
/// Boxed because it is carried around in things that are otherwise small.
pub(crate) type EncodedBlock = (Vec<u8>, Box<[u8; TEXT_GRAM_BYTES]>);

/// The conservative four-byte substring signature of one block's values.
fn block_grams(values: &[&[u8]]) -> Box<[u8; TEXT_GRAM_BYTES]> {
    let mut grams = Box::new([0_u8; TEXT_GRAM_BYTES]);
    for value in values {
        for gram in value.windows(4) {
            for bit in gram_bits(gram, TEXT_GRAM_BYTES) {
                grams[bit / 8] |= 1 << (bit % 8);
            }
        }
    }
    grams
}

/// The values of one block, given where each of them ends relative to the block.
fn block_values<'a>(ends: &[u32], bytes: &'a [u8]) -> Vec<&'a [u8]> {
    let mut out = Vec::with_capacity(ends.len());
    let mut from = 0;
    for &to in ends {
        out.push(&bytes[from..to as usize]);
        from = to as usize;
    }
    out
}

/// Encodes every block still raw at the end of a load: the part block each column ends on and,
/// for a column too small to have settled a shape, every block it has.
///
/// Across threads, the way [`encode_ready`] does it. This ran one column at a time on the thread
/// closing the table, and a column that never settled a shape encodes each block by trying every
/// candidate, so on a million rows of `hits` it was most of the load's CPU on one core.
fn finish_dictionaries(dictionaries: &mut [Option<GlobalDictionary>]) -> Result<()> {
    for dictionary in dictionaries.iter_mut().flatten() {
        if !dictionary.early.is_empty() {
            return Err(Error::internal("a dictionary block handed out never came back"));
        }
        dictionary.seal_rest();
        dictionary.settle_rest()?;
    }
    encode_waiting(dictionaries)?;
    // A block handed out and never given back leaves a gap nothing above would notice when it was
    // the last one, so the count is checked against the values as well.
    if dictionaries
        .iter()
        .flatten()
        .any(|dictionary| dictionary.encoded() != dictionary.values().div_ceil(TEXT_PAYLOAD_VALUES))
    {
        return Err(Error::internal("a dictionary block handed out never came back"));
    }
    Ok(())
}

/// Encodes the waiting blocks of every dictionary across threads, and appends them to their columns
/// in order.
fn encode_waiting(dictionaries: &mut [Option<GlobalDictionary>]) -> Result<()> {
    let jobs = dictionaries
        .iter()
        .enumerate()
        .flat_map(|(column, held)| {
            (0..held.as_ref().map_or(0, |held| held.waiting.len())).map(move |at| (column, at))
        })
        .collect::<Vec<_>>();
    if jobs.is_empty() {
        return Ok(());
    }
    let one = |column: usize, at: usize| -> Result<(usize, usize, EncodedBlock)> {
        let held = dictionaries[column].as_ref().ok_or_else(|| Error::internal("no dictionary"))?;
        Ok((column, at, held.encode_waiting(at)?))
    };
    let workers = std::thread::available_parallelism()
        .map_or(1, usize::from)
        .min(MAX_FREQUENCY_WORKERS)
        .min(jobs.len());
    let made = if workers <= 1 {
        jobs.iter().map(|&(column, at)| one(column, at)).collect::<Result<Vec<_>>>()?
    } else {
        let next = AtomicUsize::new(0);
        let jobs = &jobs;
        let pieces = std::thread::scope(|scope| {
            (0..workers)
                .map(|_| {
                    scope.spawn(|| {
                        let mut mine = Vec::new();
                        loop {
                            let job = next.fetch_add(1, Atomic::Relaxed);
                            let Some(&(column, at)) = jobs.get(job) else { break };
                            mine.push(one(column, at)?);
                        }
                        Ok(mine)
                    })
                })
                .collect::<Vec<_>>()
                .into_iter()
                .map(|handle| {
                    handle
                        .join()
                        .map_err(|_| Error::internal("a dictionary encode worker panicked"))?
                })
                .collect::<Result<Vec<_>>>()
        })?;
        pieces.into_iter().flatten().collect()
    };
    let mut done: Vec<Vec<(usize, EncodedBlock)>> =
        (0..dictionaries.len()).map(|_| Vec::new()).collect();
    for (column, at, bytes) in made {
        done[column].push((at, bytes));
    }
    for (column, mut made) in done.into_iter().enumerate() {
        if made.is_empty() {
            continue;
        }
        let Some(held) = dictionaries[column].as_mut() else { continue };
        made.sort_by_key(|(at, _)| *at);
        let waiting = std::mem::take(&mut held.waiting);
        for ((at, _), (_, block)) in waiting.into_iter().zip(made) {
            if held.encoded() != at {
                return Err(Error::internal("a dictionary block was encoded out of order"));
            }
            held.push_block(block);
        }
    }
    Ok(())
}

/// Which of [`payload_shapes`] comes out smallest over a sample of the blocks.
///
/// Every shape is encoded over the same sample and the smallest wins, which is the exhaustive
/// search moved up a level: over shapes of a column rather than over candidates of a chunk. The
/// sample is spread across the dictionary so that the first and last blocks are both in it, because
/// a dictionary written in first seen order has its common values at the front and its long tail at
/// the back, and those do not compress alike. Which blocks those are is
/// [`GlobalDictionary::seal`]'s to decide, because by the time this is called the rest of them have
/// been encoded and the raw bytes are gone.
fn settle_shape(sample: &[Vec<&[u8]>]) -> Result<chooser::Settled> {
    let mut best: Option<(chooser::Settled, usize)> = None;
    for shape in payload_shapes() {
        let mut size = 0;
        for block in sample {
            size += string::encode_with(block, &shape)?.len();
        }
        if best.as_ref().is_none_or(|(_, smallest)| size < *smallest) {
            best = Some((shape, size));
        }
    }
    best.map(|(shape, _)| shape)
        .ok_or_else(|| invalid("no shape applies to a global dictionary payload"))
}

/// The sorted order laid out the way a reader reads it, in blocks of [`TEXT_RANK_BLOCK`] entries.
///
/// Each block holds its heads first and then its codes, rather than pairing them, because a search
/// asks for a head at every probe and for a code about once a search. Keeping the heads together
/// means a probe touches eight bytes of a block rather than twelve spread over it, and the last few
/// probes of a search, which are the ones that land in the same block, touch the same cache line.
fn encode_ranks(order: &[(u64, u32)], code_bits: usize) -> Result<(Vec<u8>, Vec<u64>)> {
    let mut out = Vec::with_capacity(order.len() * 4);
    let mut ends = Vec::with_capacity(order.len().div_ceil(TEXT_RANK_BLOCK));
    let mut heads = Vec::with_capacity(TEXT_RANK_BLOCK);
    let mut codes = Vec::with_capacity(TEXT_RANK_BLOCK);
    for block in order.chunks(TEXT_RANK_BLOCK) {
        // The order is sorted by value and a head is a prefix of a value, so the heads of a block
        // rise, the smallest is the first and the largest is the last.
        let base = block.first().map_or(0, |&(head, _)| head);
        let span = block.last().map_or(0, |&(head, _)| head.wrapping_sub(base));
        let width = (u64::BITS - span.leading_zeros()) as usize;
        heads.clear();
        codes.clear();
        for &(head, code) in block {
            heads.push(head.wrapping_sub(base));
            codes.push(u64::from(code));
        }
        put_u64(&mut out, base);
        out.push(width as u8);
        bitpack::pack_tail(&heads, width, &mut out)
            .map_err(|_| invalid("global dictionary heads do not pack"))?;
        bitpack::pack_tail(&codes, code_bits, &mut out)
            .map_err(|_| invalid("global dictionary codes do not pack"))?;
        ends.push(out.len() as u64);
    }
    Ok((out, ends))
}

/// Opens a column's global dictionary, which reads its index and none of its payload.
///
/// `keep_budget` is how many decoded payload bytes this dictionary may hold on to, and every
/// caller bar the test of the ceiling passes [`TEXT_KEEP_BUDGET`]. It is a parameter rather than
/// the constant read where it is used because a test of a ceiling that cannot be moved has to build
/// a quarter of a gigabyte of dictionary to reach it.
fn open_global_dictionary(
    file: Arc<File>,
    page: Page,
    ty: &LogicalType,
    keep_budget: usize,
) -> Result<Vector> {
    if !coded_type(ty) {
        return Err(invalid("global dictionary belongs to a non-string column"));
    }
    let mut header = [0; DICTIONARY_HEADER];
    read_at(&file, page.offset, &mut header)?;
    let count = u32::from_le_bytes(header[0..4].try_into().expect("four bytes")) as usize;
    let per_block = u32::from_le_bytes(header[4..8].try_into().expect("four bytes")) as usize;
    let blocks = u32::from_le_bytes(header[8..12].try_into().expect("four bytes")) as usize;
    let width = u32::from_le_bytes(header[12..16].try_into().expect("four bytes"));
    let scattered = width & DICTIONARY_SCATTERED != 0;
    let has_grams = width & DICTIONARY_GRAMS != 0;
    let gram_width =
        if width & DICTIONARY_WIDE_GRAMS != 0 { TEXT_GRAM_BYTES } else { NARROW_GRAM_BYTES };
    let offset_bits = (width & !DICTIONARY_FLAGS) as usize;
    if per_block != TEXT_PAYLOAD_VALUES {
        return Err(invalid("global dictionary block width differs"));
    }
    if blocks != count.div_ceil(TEXT_PAYLOAD_VALUES) {
        return Err(invalid("global dictionary block count differs from its value count"));
    }
    if offset_bits > u32::BITS as usize {
        return Err(invalid("global dictionary packs offsets past a payload"));
    }
    let offset_len = offset_bytes(count, offset_bits);
    // The sorted order is kept out of the index on purpose. The index is read and checksummed in
    // full the moment the column is first touched, and the order is half again the size of the
    // offsets, so putting it there would make every query that reads a string column pay for a
    // search that most of them never make.
    let ranks = count;
    let rank_blocks = ranks.div_ceil(TEXT_RANK_BLOCK);
    // Three words a payload block, for where it starts, how long it is and what it hashes to, or
    // two of them on a file that has the blocks back to back and needs no start. Two a rank block
    // either way, since those are still one run.
    let payload_words = if scattered { 3 } else { 2 };
    let hash_len = blocks
        .checked_mul(payload_words * 8)
        .and_then(|len| len.checked_add(rank_blocks.checked_mul(16)?))
        .and_then(|len| len.checked_add(usize::from(has_grams) * 8))
        .ok_or_else(|| invalid("global dictionary block count overflow"))?;
    let gram_len = if has_grams {
        blocks
            .checked_mul(gram_width)
            .ok_or_else(|| invalid("global dictionary signature count overflow"))?
    } else {
        0
    };
    let index_len = DICTIONARY_HEADER
        .checked_add(offset_len)
        .and_then(|len| len.checked_add(hash_len))
        .ok_or_else(|| invalid("global dictionary header overflow"))?;
    if index_len > page.length as usize {
        return Err(invalid("global dictionary offset index exceeds its page"));
    }
    let mut index = vec![0; index_len];
    index[..DICTIONARY_HEADER].copy_from_slice(&header);
    read_at(&file, page.offset + DICTIONARY_HEADER as u64, &mut index[DICTIONARY_HEADER..])?;
    if checksum(&index) != page.hash {
        return Err(invalid("global dictionary index checksum differs"));
    }
    let word_end = index_len - usize::from(has_grams) * 8;
    let gram_hash = has_grams
        .then(|| u64::from_le_bytes(index[word_end..index_len].try_into().expect("eight bytes")));
    let mut words = index[DICTIONARY_HEADER + offset_len..word_end]
        .chunks_exact(8)
        .map(|part| u64::from_le_bytes(part.try_into().expect("eight bytes")))
        .collect::<Vec<_>>();
    let mut rest = words.split_off(blocks * payload_words);
    let rank_hashes = rest.split_off(rank_blocks);
    let rank_ends = rest;
    // A rank block packs its heads at whatever width its own values need, so its length is no longer
    // arithmetic on the block number and the reader has to be told where each one ends.
    if rank_ends.windows(2).any(|pair| pair[0] >= pair[1]) {
        return Err(invalid("global dictionary order blocks do not rise"));
    }
    let rank_len = usize::try_from(rank_ends.last().copied().unwrap_or_default())
        .map_err(|_| invalid("global dictionary rank overflow"))?;
    let body_len = index_len
        .checked_add(rank_len)
        .ok_or_else(|| invalid("global dictionary header overflow"))?;
    if body_len > page.length as usize {
        return Err(invalid("global dictionary order exceeds its page"));
    }
    let gram_end = body_len
        .checked_add(gram_len)
        .ok_or_else(|| invalid("global dictionary signature length overflow"))?;
    if gram_end > page.length as usize {
        return Err(invalid("global dictionary signatures exceed their page"));
    }
    let grams = gram_hash.map(|hash| NativeGrams {
        start: page.offset + body_len as u64,
        length: gram_len,
        width: gram_width,
        hash,
        verdicts: Mutex::new(Vec::new()),
    });
    // The offsets stay where they were read, behind the header, rather than being copied out. On a
    // dictionary of millions of values they are megabytes, and a copy is as many fresh pages to
    // fault in again on a query that may want a handful of strings.
    let mut offsets = index;
    offsets.truncate(DICTIONARY_HEADER + offset_len);
    let hashes = words.split_off(blocks * (payload_words - 1));
    let (starts, lengths) = if scattered {
        let mut starts = Vec::with_capacity(blocks);
        let mut lengths = Vec::with_capacity(blocks);
        for pair in words.chunks_exact(2) {
            starts.push(pair[0]);
            lengths.push(pair[1]);
        }
        (starts, lengths)
    } else {
        // A file written before the blocks said where they were has them behind one another at the
        // end of the page, so the base is where the sorted order stops and each end is the start of
        // the one after it. Turning them round here is what lets everything below take one shape.
        let base = page.offset + gram_end as u64;
        let mut starts = Vec::with_capacity(blocks);
        let mut lengths = Vec::with_capacity(blocks);
        let mut at = 0_u64;
        for &end in &words {
            let len = end
                .checked_sub(at)
                .ok_or_else(|| invalid("global dictionary block ends before it starts"))?;
            starts.push(base + at);
            lengths.push(len);
            at = end;
        }
        (starts, lengths)
    };
    // What the offsets bound is the decoded payload, and what the page length counts is the stored
    // one, so on a format 26 file the block lengths adding up to the rest of the page is the one
    // thing that ties the index to the page. From format 27 the blocks are written during the load
    // and the page is only the index and the order, so there the most that can be said is that
    // every block is somewhere in the file past its header.
    let stored_len = page.length as u64 - gram_end as u64;
    if scattered && stored_len == 0 {
        let size = file.metadata().map_err(io)?.len();
        let inside = starts.iter().zip(&lengths).all(|(&start, &len)| {
            start >= HEADER && start.checked_add(len).is_some_and(|end| end <= size)
        });
        if !inside {
            return Err(invalid("global dictionary block lies outside the file"));
        }
    } else if lengths.iter().try_fold(0_u64, |sum, len| sum.checked_add(*len)) != Some(stored_len) {
        return Err(invalid("global dictionary blocks do not bound the payload"));
    }
    Vector::external_text(
        ty.clone(),
        Arc::new(NativeText {
            file,
            values: count,
            offsets,
            offset_bits,
            value_ends: OnceLock::new(),
            value_lens: OnceLock::new(),
            ends_asked: AtomicUsize::new(0),
            ranks,
            rank_at: page.offset + index_len as u64,
            rank_ends,
            rank_hashes,
            rank_blocks: (0..rank_blocks).map(|_| OnceLock::new()).collect(),
            code_bits: code_width(count),
            code_ranks: OnceLock::new(),
            starts,
            lengths,
            hashes,
            grams,
            blocks: (0..blocks).map(|_| OnceLock::new()).collect(),
            char_lens: (0..blocks).map(|_| OnceLock::new()).collect(),
            keep_budget,
            payload_kept: AtomicUsize::new(0),
            swept: (0..blocks).map(|_| AtomicBool::new(false)).collect(),
            visit_dropped: AtomicUsize::new(0),
            searched: Mutex::new(HashMap::new()),
        }),
    )
}

/// What a stored page is, without decoding a value out of it.
///
/// Two layers, and both of them belong in the answer. The codec byte at the front of every page is
/// the format's own choice, and it is what says whether the column came back as codes into a table
/// wide dictionary, as a bit packed page, as an encoding cascade or as the bytes themselves. Under
/// the cascade codecs there is a second choice the encoder made per chunk, and that is what
/// [`integer::describe`] and [`string::describe`] already write out as `DICT(PACKED, PACKED)`.
///
/// This mirrors the tags [`decode`] reads and has to be kept beside it. A page whose header this
/// cannot walk comes back as text rather than as an error, because a caller asking what a file
/// looks like is usually asking because something is wrong with it, and a report that stops at the
/// first bad page is a report that says nothing about the other nine hundred.
fn page_encoding(ty: &LogicalType, rows: usize, bytes: &[u8]) -> String {
    /// The page header is the codec, the validity tag and, for a page that stores a mask, the mask.
    fn cascade_at(rows: usize, bytes: &[u8]) -> Result<(u8, usize)> {
        let mut cur = Cursor::new(bytes);
        let codec = cur.u8()?;
        if cur.u8()? == 2 {
            cur.take(rows.div_ceil(8))?;
        }
        Ok((codec, cur.at))
    }
    let Ok((codec, at)) = cascade_at(rows, bytes) else {
        return "UNREADABLE".to_string();
    };
    let tail = &bytes[at..];
    let described = |described: Result<String>| described.unwrap_or_else(|_| "UNREADABLE".into());
    match codec {
        0 => match ty {
            LogicalType::Varchar | LogicalType::Blob => "PLAIN".to_string(),
            _ => "FIXED".to_string(),
        },
        1 => "DICT(PLAIN)".to_string(),
        2 => "FOR+BITPACK".to_string(),
        3 => "TABLE DICT".to_string(),
        4 => format!("TABLE DICT({})", described(integer::describe(tail))),
        5 => described(integer::describe(tail)),
        6 => described(string::describe(tail)),
        other => format!("CODEC {other}"),
    }
}

/// Selected stable dictionary codes from one page.
///
/// Pair-frequency construction needs at most the bounded heavy-hitter rows. Reading those code
/// positions directly avoids materializing every code in each part that contains a candidate.
fn decode_selected_stable_codes(
    rows: usize,
    bytes: &[u8],
    positions: &[usize],
    out: &mut Vec<Option<u32>>,
) -> Result<bool> {
    if positions.windows(2).any(|pair| pair[0] >= pair[1])
        || positions.last().is_some_and(|&position| position >= rows)
    {
        return Err(invalid("selected code positions are not sorted and in range"));
    }
    let mut cur = Cursor::new(bytes);
    let codec = cur.u8()?;
    if codec != 3 && codec != 4 {
        return Ok(false);
    }
    let flag = cur.u8()?;
    let mask = match flag {
        0 | 1 => None,
        2 => {
            let at = cur.at;
            let len = rows.div_ceil(8);
            cur.take(len)?;
            Some((at, len))
        }
        _ => return Err(invalid("page validity tag differs")),
    };
    let valid = |row: usize| match flag {
        0 => true,
        1 => false,
        2 => mask.is_some_and(|(at, _)| bytes[at + row / 8] >> (row % 8) & 1 == 1),
        _ => unreachable!("the validity tag was checked"),
    };
    if codec == 4 {
        let wide = integer::decode_selected(&bytes[cur.at..], positions)?;
        for (&row, code) in positions.iter().zip(wide) {
            let code = u32::try_from(code).map_err(|_| invalid("code is not a code"))?;
            out.push(valid(row).then_some(code));
        }
        return Ok(true);
    }
    let codes_at = cur.at;
    let codes_len = rows.checked_mul(4).ok_or_else(|| invalid("page size overflow"))?;
    cur.take(codes_len)?;
    if cur.at != bytes.len() {
        return Err(invalid("global code page has trailing bytes"));
    }
    let codes = &bytes[codes_at..codes_at + codes_len];
    for &row in positions {
        let at = row.checked_mul(4).ok_or_else(|| invalid("dictionary code offset overflow"))?;
        let code = u32::from_le_bytes(
            codes[at..at + 4].try_into().map_err(|_| invalid("dictionary code is truncated"))?,
        );
        out.push(valid(row).then_some(code));
    }
    Ok(true)
}

/// [`decode`] of only the rows at `positions`, which rise.
///
/// A compressed text page decompresses only those rows, see [`string::decode_flat_at`], and checks
/// only those rows are text. Every other page is decoded whole and gathered, since its values are
/// fixed width or its strings are shared through a dictionary, and there picking comes after.
fn decode_at(
    ty: &LogicalType,
    rows: usize,
    bytes: &[u8],
    global: Option<Arc<Vector>>,
    positions: &[u32],
) -> Result<Vector> {
    if positions.last().is_some_and(|&last| last as usize >= rows) {
        return Err(invalid("a position is past the end of the part"));
    }
    if bytes.first() != Some(&6) {
        return decode(ty, rows, bytes, global)?.gather(positions);
    }
    if !coded_type(ty) {
        return Err(invalid("compressed text codec belongs to a non-string page"));
    }
    let mut cur = Cursor::new(bytes);
    cur.u8()?;
    let validity = match cur.u8()? {
        0 => Validity::AllValid,
        1 => Validity::AllInvalid,
        2 => {
            let mask = cur.take(rows.div_ceil(8))?;
            Validity::from_iter(positions.len(), |at| {
                let row = positions[at] as usize;
                mask[row / 8] >> (row % 8) & 1 == 1
            })
        }
        _ => return Err(invalid("page validity tag differs")),
    };
    let (payload, ends) = string::decode_flat_at(&bytes[cur.at..], positions)?.into_parts();
    let mut values = StringColumn::over(Buffer::from_vec(payload).into_page());
    let mut start = 0;
    for end in ends {
        let len = end
            .checked_sub(start)
            .ok_or_else(|| invalid("compressed text value ends before it starts"))?;
        push_value(&mut values, ty, start, len)?;
        start = end;
    }
    Ok(Vector::flat(ty.clone(), Data::Varlen(values))?.with_validity(validity))
}

/// One value of a string or blob page, found in the page's payload. A varchar is checked for text
/// on the way in and a blob is not, since a blob never claimed to hold any.
fn push_value(values: &mut StringColumn, ty: &LogicalType, at: usize, len: usize) -> Result<()> {
    if ty == &LogicalType::Varchar {
        values.push_in_place(at, len)?;
    } else {
        values.push_bytes_in_place(at, len)?;
    }
    Ok(())
}

fn decode(
    ty: &LogicalType,
    rows: usize,
    bytes: &[u8],
    global: Option<Arc<Vector>>,
) -> Result<Vector> {
    let mut cur = Cursor::new(bytes);
    let codec = cur.u8()?;
    let flag = cur.u8()?;
    let validity = match flag {
        0 => Validity::AllValid,
        1 => Validity::AllInvalid,
        2 => {
            let mask = cur.take(rows.div_ceil(8))?;
            Validity::from_iter(rows, |row| mask[row / 8] >> (row % 8) & 1 == 1)
        }
        _ => return Err(invalid("page validity tag differs")),
    };
    if codec == 1 {
        if !coded_type(ty) {
            return Err(invalid("dictionary codec belongs to a non-string page"));
        }
        let count = cur.u32()? as usize;
        let payload_len = cur.u32()? as usize;
        let offset_bytes = cur.take(
            (count + 1)
                .checked_mul(4)
                .ok_or_else(|| invalid("dictionary offset count overflow"))?,
        )?;
        let offsets = offset_bytes
            .chunks_exact(4)
            .map(|part| u32::from_le_bytes(part.try_into().expect("four bytes")))
            .collect::<Vec<_>>();
        let payload = cur.take(payload_len)?.to_vec();
        if offsets.first() != Some(&0)
            || offsets.last().copied().map(|last| last as usize) != Some(payload.len())
            || offsets.windows(2).any(|pair| pair[0] > pair[1])
        {
            return Err(invalid("dictionary offsets do not bound the payload"));
        }
        // A page, because every chunk cut out of this dictionary points at the same payload and a
        // page is what lets a cut be the views and nothing else.
        let mut strings = StringColumn::over(Buffer::from_vec(payload).into_page());
        for pair in offsets.windows(2) {
            push_value(&mut strings, ty, pair[0] as usize, (pair[1] - pair[0]) as usize)?;
        }
        let mut codes = Vec::with_capacity(rows);
        for _ in 0..rows {
            codes.push(cur.u32()?);
        }
        if codes.iter().any(|code| *code as usize >= count) {
            return Err(invalid("dictionary code is out of range"));
        }
        if cur.at != bytes.len() {
            return Err(invalid("dictionary page has trailing bytes"));
        }
        let dictionary = Vector::flat(ty.clone(), Data::Varlen(strings))?;
        return Ok(Vector::dictionary(codes, dictionary)?.with_validity(validity));
    }
    if codec == 3 || codec == 4 {
        let dictionary = global.ok_or_else(|| invalid("global code page has no dictionary"))?;
        let codes = if codec == 4 {
            // The cascade holds the whole tail of the page and says how long it is itself, so the
            // check that nothing is left over is the one the decoder already makes.
            let wide = integer::decode(&bytes[cur.at..])?;
            if wide.len() != rows {
                return Err(invalid("encoded code page holds the wrong number of rows"));
            }
            // Checked once for the page rather than a fallible conversion per code. Every code a
            // file holds is inside a `u32` or the file is corrupt, so or the codes together and the
            // answer has a bit set above the low thirty two, or the sign bit, exactly when one of
            // them did. The or and the narrowing are two passes because each is then a vector
            // loop. As one loop with a `push` a code, the length check and the store kept it scalar,
            // and it was sixteen instructions a row on the two flag columns of q1.
            let seen = wide.iter().fold(0_i64, |seen, &code| seen | code);
            if seen < 0 || seen > i64::from(u32::MAX) {
                return Err(invalid("code is not a code"));
            }
            wide.iter().map(|&code| code as u32).collect()
        } else {
            let mut codes = Vec::with_capacity(rows);
            for _ in 0..rows {
                codes.push(cur.u32()?);
            }
            if cur.at != bytes.len() {
                return Err(invalid("global code page has trailing bytes"));
            }
            codes
        };
        let highest = codes.iter().copied().max();
        return Ok(Vector::stable_dictionary_validated(codes, dictionary, highest)?
            .with_validity(validity));
    }
    if codec == 6 {
        if !coded_type(ty) {
            return Err(invalid("compressed text codec belongs to a non-string page"));
        }
        // As codec 5, the layer holds the whole tail of the page and says how long it is itself.
        // It comes back as one buffer with the values laid end to end and where each one ends, which
        // is the raw form's layout, so what is left to do here is what codec 0 does.
        let (payload, ends) = string::decode_flat(&bytes[cur.at..])?.into_parts();
        if ends.len() != rows {
            return Err(invalid("compressed text page holds the wrong number of rows"));
        }
        // A page, because this is read once and handed out a chunk at a time, and a cut of a paged
        // payload moves views rather than bytes.
        let mut values = StringColumn::over(Buffer::from_vec(payload).into_page());
        let mut start = 0;
        for end in ends {
            let len = end
                .checked_sub(start)
                .ok_or_else(|| invalid("compressed text value ends before it starts"))?;
            push_value(&mut values, ty, start, len)?;
            start = end;
        }
        return Ok(Vector::flat(ty.clone(), Data::Varlen(values))?.with_validity(validity));
    }
    if codec == 5 {
        // The cascade holds the whole tail of the page and says how long it is itself.
        let values = integer::decode(&bytes[cur.at..])?;
        if values.len() != rows {
            return Err(invalid("cascade page holds the wrong number of rows"));
        }
        let data = narrowed(ty, values)?;
        return Ok(Vector::flat(ty.clone(), data)?.with_validity(validity));
    }
    if codec == 2 {
        let width = u32::from(cur.u8()?);
        let base = i128::from_le_bytes(cur.take(16)?.try_into().expect("sixteen bytes"));
        let count = cur.u32()? as usize;
        let length = count.checked_mul(8).ok_or_else(|| invalid("packed page is too long"))?;
        let words: Vec<u64> = cur
            .take(length)?
            .chunks_exact(8)
            .map(|word| u64::from_le_bytes(word.try_into().expect("eight bytes")))
            .collect();
        if cur.at != bytes.len() {
            return Err(invalid("packed page has trailing bytes"));
        }
        return Ok(Vector::packed(ty.clone(), words, width, base, rows)?.with_validity(validity));
    }
    if codec != 0 {
        return Err(invalid("page codec is unknown"));
    }
    let data = match ty {
        LogicalType::TinyInt => {
            let values = cur.take(rows)?;
            Data::Int8(values.iter().map(|item| *item as i8).collect::<Vec<_>>().into())
        }
        LogicalType::UTinyInt => Data::UInt8(cur.take(rows)?.to_vec().into()),
        LogicalType::SmallInt => {
            let values =
                cur.take(rows.checked_mul(2).ok_or_else(|| invalid("page size overflow"))?)?;
            Data::Int16(
                values
                    .chunks_exact(2)
                    .map(|item| i16::from_le_bytes(item.try_into().expect("two bytes")))
                    .collect::<Vec<_>>()
                    .into(),
            )
        }
        LogicalType::USmallInt => {
            let values =
                cur.take(rows.checked_mul(2).ok_or_else(|| invalid("page size overflow"))?)?;
            Data::UInt16(
                values
                    .chunks_exact(2)
                    .map(|item| u16::from_le_bytes(item.try_into().expect("two bytes")))
                    .collect::<Vec<_>>()
                    .into(),
            )
        }
        LogicalType::UInteger => {
            let values =
                cur.take(rows.checked_mul(4).ok_or_else(|| invalid("page size overflow"))?)?;
            Data::UInt32(
                values
                    .chunks_exact(4)
                    .map(|item| u32::from_le_bytes(item.try_into().expect("four bytes")))
                    .collect::<Vec<_>>()
                    .into(),
            )
        }
        LogicalType::UBigInt => {
            let values =
                cur.take(rows.checked_mul(8).ok_or_else(|| invalid("page size overflow"))?)?;
            Data::UInt64(
                values
                    .chunks_exact(8)
                    .map(|item| u64::from_le_bytes(item.try_into().expect("eight bytes")))
                    .collect::<Vec<_>>()
                    .into(),
            )
        }
        LogicalType::Integer | LogicalType::Date => {
            let values =
                cur.take(rows.checked_mul(4).ok_or_else(|| invalid("page size overflow"))?)?;
            Data::Int32(
                values
                    .chunks_exact(4)
                    .map(|item| i32::from_le_bytes(item.try_into().expect("four bytes")))
                    .collect::<Vec<_>>()
                    .into(),
            )
        }
        LogicalType::BigInt
        | LogicalType::Timestamp
        | LogicalType::Time
        | LogicalType::TimeTz
        | LogicalType::TimestampTz
        | LogicalType::TimestampS
        | LogicalType::TimestampMs
        | LogicalType::TimestampNs => {
            let values =
                cur.take(rows.checked_mul(8).ok_or_else(|| invalid("page size overflow"))?)?;
            Data::Int64(
                values
                    .chunks_exact(8)
                    .map(|item| i64::from_le_bytes(item.try_into().expect("eight bytes")))
                    .collect::<Vec<_>>()
                    .into(),
            )
        }
        LogicalType::HugeInt | LogicalType::Uuid => {
            let values =
                cur.take(rows.checked_mul(16).ok_or_else(|| invalid("page size overflow"))?)?;
            Data::Int128(
                values
                    .chunks_exact(16)
                    .map(|item| i128::from_le_bytes(item.try_into().expect("sixteen bytes")))
                    .collect::<Vec<_>>()
                    .into(),
            )
        }
        LogicalType::UHugeInt => {
            let values =
                cur.take(rows.checked_mul(16).ok_or_else(|| invalid("page size overflow"))?)?;
            Data::UInt128(
                values
                    .chunks_exact(16)
                    .map(|item| u128::from_le_bytes(item.try_into().expect("sixteen bytes")))
                    .collect::<Vec<_>>()
                    .into(),
            )
        }
        LogicalType::Float => {
            let values =
                cur.take(rows.checked_mul(4).ok_or_else(|| invalid("page size overflow"))?)?;
            Data::Float32(
                values
                    .chunks_exact(4)
                    .map(|item| f32::from_le_bytes(item.try_into().expect("four bytes")))
                    .collect::<Vec<_>>()
                    .into(),
            )
        }
        LogicalType::Double => {
            let values =
                cur.take(rows.checked_mul(8).ok_or_else(|| invalid("page size overflow"))?)?;
            Data::Float64(
                values
                    .chunks_exact(8)
                    .map(|item| f64::from_le_bytes(item.try_into().expect("eight bytes")))
                    .collect::<Vec<_>>()
                    .into(),
            )
        }
        LogicalType::Interval => {
            let values =
                cur.take(rows.checked_mul(16).ok_or_else(|| invalid("page size overflow"))?)?;
            Data::Interval(
                values
                    .chunks_exact(16)
                    .map(|item| {
                        (
                            i32::from_le_bytes(item[..4].try_into().expect("four bytes")),
                            i32::from_le_bytes(item[4..8].try_into().expect("four bytes")),
                            i64::from_le_bytes(item[8..].try_into().expect("eight bytes")),
                        )
                    })
                    .collect::<Vec<_>>()
                    .into(),
            )
        }
        LogicalType::Boolean => {
            let values = cur.take(rows)?;
            if values.iter().any(|value| *value > 1) {
                return Err(invalid("boolean page has another value"));
            }
            Data::Bool(values.iter().map(|value| *value == 1).collect::<Vec<_>>().into())
        }
        // Whichever integer the declared width says, which is the mapping the rest of the engine
        // already uses for a decimal in memory.
        LogicalType::Decimal { .. } => match ty.physical() {
            PhysicalType::Int16 => {
                let values =
                    cur.take(rows.checked_mul(2).ok_or_else(|| invalid("page size overflow"))?)?;
                Data::Int16(
                    values
                        .chunks_exact(2)
                        .map(|item| i16::from_le_bytes(item.try_into().expect("two bytes")))
                        .collect::<Vec<_>>()
                        .into(),
                )
            }
            PhysicalType::Int32 => {
                let values =
                    cur.take(rows.checked_mul(4).ok_or_else(|| invalid("page size overflow"))?)?;
                Data::Int32(
                    values
                        .chunks_exact(4)
                        .map(|item| i32::from_le_bytes(item.try_into().expect("four bytes")))
                        .collect::<Vec<_>>()
                        .into(),
                )
            }
            PhysicalType::Int64 => {
                let values =
                    cur.take(rows.checked_mul(8).ok_or_else(|| invalid("page size overflow"))?)?;
                Data::Int64(
                    values
                        .chunks_exact(8)
                        .map(|item| i64::from_le_bytes(item.try_into().expect("eight bytes")))
                        .collect::<Vec<_>>()
                        .into(),
                )
            }
            _ => {
                let values =
                    cur.take(rows.checked_mul(16).ok_or_else(|| invalid("page size overflow"))?)?;
                Data::Int128(
                    values
                        .chunks_exact(16)
                        .map(|item| i128::from_le_bytes(item.try_into().expect("sixteen bytes")))
                        .collect::<Vec<_>>()
                        .into(),
                )
            }
        },
        LogicalType::Varchar | LogicalType::Blob | LogicalType::Bit => {
            let offset_bytes = cur
                .take((rows + 1).checked_mul(4).ok_or_else(|| invalid("offset count overflow"))?)?;
            let offsets = offset_bytes
                .chunks_exact(4)
                .map(|part| u32::from_le_bytes(part.try_into().expect("four bytes")))
                .collect::<Vec<_>>();
            let payload = cur.take(bytes.len() - cur.at)?.to_vec();
            if offsets.first() != Some(&0)
                || offsets.last().copied().map(|last| last as usize) != Some(payload.len())
                || offsets.windows(2).any(|pair| pair[0] > pair[1])
            {
                return Err(invalid("string offsets do not bound the payload"));
            }
            // A page for the reason the dictionary payload above is one: the page is read once and
            // handed out a chunk at a time, and a cut of a paged payload moves views rather than
            // bytes.
            //
            // A varchar is checked for text on the way in and a blob and a bit string are not,
            // because the second pair never claimed to hold any. Reading them through the checking
            // seam would refuse a column for holding exactly what it was told to hold.
            let mut values = StringColumn::over(Buffer::from_vec(payload).into_page());
            let text = ty == &LogicalType::Varchar;
            for pair in offsets.windows(2) {
                let (at, len) = (pair[0] as usize, (pair[1] - pair[0]) as usize);
                if text {
                    values.push_in_place(at, len)?;
                } else {
                    values.push_bytes_in_place(at, len)?;
                }
            }
            Data::Varlen(values)
        }
        _ => return Err(Error::not_implemented(format!("native page for {ty}"))),
    };
    if cur.at != bytes.len() {
        return Err(invalid("page has trailing bytes"));
    }
    Ok(Vector::flat(ty.clone(), data)?.with_validity(validity))
}

#[cfg(test)]
mod tests {
    use std::fs::{self, OpenOptions};
    use std::io::{Seek, SeekFrom, Write};
    use std::path::PathBuf;
    use std::time::{SystemTime, UNIX_EPOCH};

    use rudb_common::Stat;
    use rudb_common::Value;
    use rudb_common::bounds::{Frequencies, Op, Remainder, Zones};
    use rudb_common::stat::Provenance;

    use super::*;

    #[test]
    fn a_name_taken_in_pieces_is_the_name_of_the_pieces_joined() {
        let bytes: Vec<u8> =
            (0..300_u32).map(|at| (at.wrapping_mul(2_654_435_761) >> 13) as u8).collect();
        for length in [0, 1, 7, 31, 32, 33, 63, 64, 65, 100, 300] {
            let whole = content_name(&bytes[..length]);
            for step in [1, 3, 8, 31, 32, 33, 64, 301] {
                let mut namer = ContentNamer::default();
                bytes[..length].chunks(step).for_each(|piece| namer.update(piece));
                assert_eq!(namer.finish(), whole, "{length} bytes in pieces of {step}");
            }
        }
    }

    /// The chooser as it was before it could rule kinds out up front: the same narrowing, with every
    /// kind tested for. What it writes is what the file used to hold.
    #[derive(Debug)]
    struct TestsEverything<'a>(&'a dyn chooser::Chooser);

    impl chooser::Chooser for TestsEverything<'_> {
        fn name(&self) -> &'static str {
            "tests everything"
        }

        fn narrow_strings(
            &self,
            values: &[&[u8]],
            offered: &[string::Kind],
            depth: u8,
        ) -> Vec<string::Kind> {
            self.0.narrow_strings(values, offered, depth)
        }

        fn narrow_integers(
            &self,
            values: &[i64],
            offered: &[integer::Kind],
            depth: u8,
        ) -> Vec<integer::Kind> {
            self.0.narrow_integers(values, offered, depth)
        }
    }

    #[test]
    fn ruling_kinds_out_before_testing_for_them_writes_the_same_bytes() {
        let columns: Vec<Vec<i64>> = vec![
            vec![],
            vec![5; 1000],
            (0..1000).collect(),
            (0..1000).map(|row| 1_600_000_000_000_000 + row * 1_000_000).collect(),
            (0..1000).map(|row| row / 50).collect(),
            (0..1000).map(|row| if row % 97 == 0 { row } else { 0 }).collect(),
            (0..1000).map(|row| (row * 7919) % 13).collect(),
            (0..1000).map(|row| (row * 2_654_435_761) % 1_000_003).collect(),
            (0..1000).map(|row| [3, 3, 3, 9, 9, 1][row as usize % 6]).collect(),
            (0..1000).map(|row| i64::MIN + row % 3).collect(),
        ];
        let choosers: [&dyn chooser::Chooser; 2] = [&Fixed, &Codes];
        for column in &columns {
            for chooser in choosers {
                let quick = integer::encode_with(column, chooser).unwrap();
                let full = integer::encode_with(column, &TestsEverything(chooser)).unwrap();
                assert_eq!(
                    quick,
                    full,
                    "{} on {:?}",
                    chooser.name(),
                    &column[..column.len().min(8)]
                );
            }
        }
    }

    /// Parts of a column that all look alike come out of a settled shape byte for byte as they
    /// come out of a search, because the search would have kept the same tree on every one.
    #[test]
    fn parts_that_look_alike_replay_to_the_bytes_a_search_writes() {
        let mut settling = Settling::default();
        for part in 0..STRIPE_PARTS as i64 {
            let values: Vec<i64> = (0..2048)
                .map(|row| 1_600_000_000_000_000 + (part * 2048 + row) * 1_000_000 + row % 7)
                .collect();
            let searched = integer::encode_with(&values, &Fixed).unwrap();
            assert_eq!(settling.encode(&values).unwrap(), searched, "part {part}");
        }
    }

    /// A column that changes shape partway through a stripe still reads back, and no part comes
    /// out much bigger than a search would have made it, because a replay that stops fitting or
    /// grows past a quarter a row is searched.
    #[test]
    fn a_column_that_changes_under_the_shape_is_searched_again() {
        let mut state = 0x9e37_79b9_7f4a_7c15_u64;
        let mut noise = move || {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            (state % 1_000_000) as i64
        };
        let mut settling = Settling::default();
        for part in 0..STRIPE_PARTS as i64 {
            let values: Vec<i64> = match part / 16 {
                0 => (0..2048).map(|row| (part * 2048 + row) / 300).collect(),
                1 => (0..2048).map(|_| noise()).collect(),
                2 => (0..2048).map(|row| if row % 97 == 0 { row } else { 42 }).collect(),
                _ => (0..2048).map(|row| 5 + (part * 2048 + row) * 1_000_000).collect(),
            };
            let settled = settling.encode(&values).unwrap();
            assert_eq!(integer::decode(&settled).unwrap(), values, "part {part}");
            let searched = integer::encode_with(&values, &Fixed).unwrap();
            assert!(
                settled.len() * 4 <= searched.len() * 5,
                "part {part}: {} settled against {} searched, {} against {}",
                settled.len(),
                searched.len(),
                integer::describe(&settled).unwrap(),
                integer::describe(&searched).unwrap(),
            );
        }
    }

    #[test]
    fn checksum_matches_fixed_vectors() {
        assert_eq!(checksum(b""), 0xef46_db37_51d8_e999);
        assert_eq!(checksum(b"a"), 0xd24e_c4f1_a98c_6e5b);
        assert_eq!(checksum(b"abc"), 0x44bc_2cf5_ad77_0999);
    }

    #[test]
    fn sorting_across_threads_matches_sorting_on_one() {
        let mut state = 0x9e37_79b9_7f4a_7c15_u64;
        let mut next = move || {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            state
        };
        let mut values = Vec::new();
        for at in 0..150_000_u64 {
            let value = match next() % 6 {
                0 => Vec::new(),
                1 => format!("https://example.com/{}", next() % 5_000).into_bytes(),
                2 => format!("https://example.com/path/{at}").into_bytes(),
                3 => b"same".to_vec(),
                4 => vec![0xff; (next() % 12) as usize],
                _ => (0..next() % 20).map(|_| (next() % 3) as u8).collect(),
            };
            values.push(value);
        }
        let value = |code: u32| values[code as usize].as_slice();
        for workers in [1, 2, 3, 8, 32] {
            let mut one = (0..values.len() as u32).rev().collect::<Vec<_>>();
            let mut across = one.clone();
            sort_by_value(&mut one, value);
            sort_by_value_across(&mut across, value, workers);
            assert_eq!(one, across, "{workers} workers");
        }
        let mut sorted = (0..values.len() as u32).collect::<Vec<_>>();
        sort_by_value_across(&mut sorted, value, 8);
        assert!(sorted.windows(2).all(|pair| value(pair[0]) <= value(pair[1])));
    }

    fn path(label: &str) -> PathBuf {
        let stamp = SystemTime::now().duration_since(UNIX_EPOCH).expect("time advances").as_nanos();
        std::env::temp_dir().join(format!("rudb-native-{label}-{}-{stamp}.rdb", std::process::id()))
    }

    /// Every value of a dictionary in code order, which the tests have no other way to ask for now
    /// that a dictionary does not keep the bytes of the values it has seen.
    ///
    /// Only valid once `finish_blocks` has run, because until then the last part block is still raw.
    fn dictionary_values(dictionary: &GlobalDictionary) -> Vec<Vec<u8>> {
        let (flat, bases) = dictionary.decoded(None).expect("the blocks decode");
        (0..dictionary.values())
            .map(|code| {
                let (from, to) = GlobalDictionary::value_span(&dictionary.ends, &bases, code);
                flat[from..to].to_vec()
            })
            .collect()
    }

    /// The sections a test put in the table, which is every one the writer did not.
    ///
    /// A table now carries a summary and a sketch per column out of the write itself, and a test
    /// about the section table is not about those. Filtering by kind rather than by count, so a
    /// table that turns out to have no room for its summaries does not quietly change what these
    /// tests are asserting over.
    fn attached(table: &Table) -> Vec<&Section> {
        table.sections().iter().filter(|held| !held.among(section::STATISTICS_KINDS)).collect()
    }

    /// A read names the offset it wants, so a cursor somebody else moved cannot reach it.
    #[test]
    fn a_read_at_an_offset_ignores_where_another_thread_left_the_cursor() {
        const SPANS: usize = 64;
        const SPAN: usize = 512;
        let path = path("positional");
        let content: Vec<u8> =
            (0..SPANS).flat_map(|span| std::iter::repeat_n(span as u8, SPAN)).collect();
        fs::write(&path, &content).expect("the file is written");
        let file = Arc::new(File::open(&path).expect("the file opens"));
        std::thread::scope(|scope| {
            for _ in 0..8 {
                let file = Arc::clone(&file);
                scope.spawn(move || {
                    for _ in 0..64 {
                        for span in 0..SPANS {
                            let mut bytes = [0_u8; SPAN];
                            read_at(&file, (span * SPAN) as u64, &mut bytes)
                                .expect("the span reads");
                            assert!(
                                bytes.iter().all(|byte| *byte == span as u8),
                                "span {span} came back as {}",
                                bytes[0],
                            );
                        }
                    }
                });
            }
        });
        let mut past = [0_u8; SPAN];
        let end = (SPANS * SPAN) as u64;
        let error = read_at(&file, end, &mut past).expect_err("a read past the end is refused");
        assert!(error.message().contains("ends before its declared length"), "{error}");
        drop(file);
        let _ = fs::remove_file(&path);
    }

    /// The writer records where it put a page and puts it there.
    ///
    /// This used to move the file's cursor between the steps that record an offset, which is what
    /// reading the pages back to build the frequencies did on a platform with no `pread`, and the
    /// directory landed on top of a page. The writer's file is an `rudb_io` file now and has no
    /// cursor to move, so what is left is the check that every page is where the directory says.
    #[test]
    fn a_writer_puts_a_page_where_it_said_it_did_wherever_the_cursor_has_got_to() {
        let path = path("cursor");
        let mut writer = Writer::create(
            &path,
            "items",
            vec![
                Field::required("id", LogicalType::Integer),
                Field::new("text", LogicalType::Varchar),
            ],
        )
        .expect("new file");
        writer.append(&sample()).expect("first part");
        writer.append(&sample()).expect("second part");
        writer.finish().expect("commit");
        let reader = Reader::open(&path).expect("reopen from disk");
        assert_eq!(reader.table().rows(), 6);
        let ids = reader.read(0, &[0]).expect("the integer page reads back");
        assert_eq!(ids.value_at(0, 0), Value::Integer(4));
        assert_eq!(ids.value_at(2, 0), Value::Integer(-2));
        let text = reader.read(1, &[1]).expect("the text page reads back");
        assert_eq!(text.value_at(1, 0), Value::Null);
        assert_eq!(text.value_at(2, 0), Value::Varchar("long text after a slash".into()));
        // Nothing the directory points at may run past the end of the file, which is the shape the
        // failure took: a page recorded at an offset the directory had already been written over.
        let end = reader.table().stripes().iter().flat_map(|stripe| {
            stripe
                .pages
                .iter()
                .map(|page| page.offset + u64::from(page.length))
                .chain(std::iter::once(stripe.index.offset + u64::from(stripe.index.length)))
        });
        let last = end.fold(HEADER, u64::max);
        let directory = fs::metadata(&path).expect("the file is there").len();
        assert!(last <= directory, "a page runs to {last} in a file of {directory} bytes");
        fs::remove_file(path).expect("remove scratch file");
    }

    /// How long a global dictionary index is, read out of the page's own header.
    ///
    /// The tests below damage a byte of the order or of the payload, so they need to know where each
    /// one starts, and working it out here rather than writing a number down means adding something
    /// to the index does not quietly turn one of them into a test that damages the index instead.
    fn dictionary_index_len(header: &[u8; DICTIONARY_HEADER]) -> u64 {
        let count = u64::from(u32::from_le_bytes(header[0..4].try_into().expect("four bytes")));
        let blocks = u64::from(u32::from_le_bytes(header[8..12].try_into().expect("four bytes")));
        let width = u32::from_le_bytes(header[12..16].try_into().expect("four bytes"));
        let bits = (width & !DICTIONARY_FLAGS) as usize;
        let payload_words = if width & DICTIONARY_SCATTERED == 0 { 2 } else { 3 };
        let rank_blocks = count.div_ceil(TEXT_RANK_BLOCK as u64);
        DICTIONARY_HEADER as u64
            + offset_bytes(count as usize, bits) as u64
            + blocks * payload_words * 8
            + rank_blocks * 16
            + if width & DICTIONARY_GRAMS == 0 { 0 } else { 8 }
    }

    fn sample() -> Chunk {
        Chunk::new(vec![
            Vector::from_values(
                LogicalType::Integer,
                &[Value::Integer(4), Value::Integer(9), Value::Integer(-2)],
            )
            .expect("integers"),
            Vector::from_values(
                LogicalType::Varchar,
                &[
                    Value::Varchar("alpha".into()),
                    Value::Null,
                    Value::Varchar("long text after a slash".into()),
                ],
            )
            .expect("strings"),
        ])
        .expect("matching rows")
    }

    fn sample_ids() -> Chunk {
        Chunk::new(vec![
            Vector::flat(LogicalType::Integer, Data::Int32(vec![7, 8, 9].into()))
                .expect("integers"),
        ])
        .expect("one column")
    }

    #[test]
    fn the_planner_gets_the_null_count_off_the_same_directory_the_bounds_are_in() {
        // Six rows, two of them null. `IS NULL` used to get the same fifth any unreadable
        // condition gets, and the number was in the stripe entry next to the bounds all along.
        let path = path("nulls_for_the_planner");
        let mut writer =
            Writer::create(&path, "items", vec![Field::new("a", LogicalType::Integer)])
                .expect("new file");
        let rows = Chunk::new(vec![
            Vector::from_values(
                LogicalType::Integer,
                &[
                    Value::Integer(4),
                    Value::Null,
                    Value::Integer(9),
                    Value::Null,
                    Value::Integer(1),
                    Value::Integer(2),
                ],
            )
            .expect("integers"),
        ])
        .expect("one column");
        writer.append(&rows).expect("the only part");
        writer.finish().expect("commit");
        let reader = Reader::open(&path).expect("reopen from disk");
        let stripes = Stripes::new(reader);
        let column = stripes.column("a").expect("the file has that column");
        assert_eq!(stripes.nulls(column), Stat::exact(2, Provenance::NullCount));
        // A column the file does not have. Zero here would be a fact about a column that is not
        // there, which the planner would then divide by.
        assert_eq!(stripes.nulls(column + 1), Stat::Unknown);
        fs::remove_file(&path).expect("clean up");
    }

    #[test]
    fn the_planner_gets_a_leading_count_without_a_complete_numeric_synopsis() {
        // Six rows hold three values. The two leading counts help equality planning, while the
        // omitted value keeps the directory from being a complete grouped-count result.
        let path = path("frequencies_for_the_planner");
        let mut writer =
            Writer::create(&path, "items", vec![Field::required("id", LogicalType::Integer)])
                .expect("new file");
        let rows = Chunk::new(vec![
            Vector::from_values(
                LogicalType::Integer,
                &[
                    Value::Integer(4),
                    Value::Integer(4),
                    Value::Integer(4),
                    Value::Integer(9),
                    Value::Integer(9),
                    Value::Integer(1),
                ],
            )
            .expect("integers"),
        ])
        .expect("one column");
        writer.append(&rows).expect("the only part");
        writer.finish().expect("commit");
        let reader = Reader::open(&path).expect("reopen from disk");
        let common = Common::new(reader);
        assert_eq!(common.rows(), 6);
        let column = common.column("id").expect("the file has that column");
        assert_eq!(common.column("nothing"), None);
        assert_eq!(
            common.rows_with(column, &Bound::Int(4)),
            Stat::exact(3, Provenance::FrequencySynopsis)
        );
        // An absent value cannot be distinguished from the omitted one by the synopsis.
        assert_eq!(common.rows_with(column, &Bound::Int(7)), Stat::Unknown);
        // A constant of another domain against an integer column. Nothing in the list compares
        // with it, so the zero above would be an artefact of the mismatch rather than a fact.
        assert_eq!(common.rows_with(column, &Bound::Bytes(b"four".to_vec())), Stat::Unknown);
        assert!(common.remainder(column).is_some());
        fs::remove_file(&path).expect("clean up");
    }

    #[test]
    fn string_frequency_estimates_do_not_open_the_global_dictionary() {
        let path = path("string_frequencies_for_the_planner");
        let mut writer =
            Writer::create(&path, "items", vec![Field::required("text", LogicalType::Varchar)])
                .expect("new file");
        let rows = Chunk::new(vec![
            Vector::from_values(
                LogicalType::Varchar,
                &[
                    Value::Varchar(String::new()),
                    Value::Varchar("alpha".into()),
                    Value::Varchar(String::new()),
                    Value::Varchar("beta".into()),
                    Value::Varchar(String::new()),
                ],
            )
            .expect("strings"),
        ])
        .expect("one column");
        writer.append(&rows).expect("the only part");
        writer.finish().expect("commit");

        let reader = Reader::open(&path).expect("reopen from disk");
        assert_eq!(reader.reads().dictionaries, 0, "open reads only the directory");
        let common = Common::new(reader.clone());
        let column = common.column("text").expect("the file has that column");
        assert_eq!(
            common.rows_with(column, &Bound::Bytes(Vec::new())),
            Stat::exact(3, Provenance::FrequencySynopsis)
        );
        assert_eq!(
            common.rows_with(column, &Bound::Bytes(b"missing".to_vec())),
            Stat::exact(0, Provenance::FrequencySynopsis)
        );
        assert_eq!(
            reader.reads().dictionaries,
            0,
            "the bounded spellings answer without opening the dictionary index"
        );
        fs::remove_file(&path).expect("clean up");
    }

    #[test]
    fn host_groups_certify_omitted_hosts_and_keep_exact_aggregates() {
        let path = path("certified_host_groups");
        let mut writer =
            Writer::create(&path, "hits", vec![Field::required("Referer", LogicalType::Varchar)])
                .expect("new file");
        let mut values = vec![Value::Varchar("http://www.example.com/a".into()); 150];
        values.extend(vec![Value::Varchar("https://example.com/b".into()); 70]);
        values.extend((0..550).map(|at| Value::Varchar(format!("https://site{at}.test/x"))));
        values.push(Value::Varchar(String::new()));
        for part in values.chunks(512) {
            writer
                .append(
                    &Chunk::new(vec![
                        Vector::from_values(LogicalType::Varchar, part).expect("strings"),
                    ])
                    .expect("one column"),
                )
                .expect("part written");
        }
        writer.finish().expect("commit");
        let reader = Reader::open(&path).expect("reopen");
        assert!(reader.table.host_groups.is_none(), "no query-specific host result is stored");
        fs::remove_file(&path).expect("clean up");
    }

    /// A table directory with nothing in it but a name and one column, for the section tests.
    ///
    /// The section table is orthogonal to everything else in a directory, so the tests that pin it
    /// say so by starting from the emptiest table that encodes.
    fn bare_table(sections: Vec<Section>) -> Table {
        Table {
            name: "linked".to_owned(),
            fields: vec![Field::required("id", LogicalType::Integer)],
            stripes: Vec::new(),
            rows: 0,
            dictionaries: vec![None],
            dictionary_payloads: Vec::new(),
            demoted: Vec::new(),
            distincts: vec![None],
            frequencies: vec![None],
            pair_frequencies: Vec::new(),
            frequency_texts: Vec::new(),
            host_groups: None,
            clustering: None,
            generation: 1,
            sections,
        }
    }

    fn a_key_map_section() -> Section {
        Section {
            kind: *section::KEY_MAP,
            id: 1,
            generation: 3,
            extents: 1,
            extent_page: HEADER,
            extent_bytes: section::EXTENT_BYTES as u32,
            hash: 0x1234_5678_9abc_def0,
            flags: 0,
            header_bytes: 24,
        }
    }

    #[test]
    fn a_section_table_round_trips_through_a_directory() {
        let mut later = a_key_map_section();
        later.kind = *b"RUDBZZ9\0";
        later.id = 2;
        let table = bare_table(vec![a_key_map_section(), later]);
        let directory = encode_directory(&table).expect("directory");
        let decoded = decode_directory(&directory, 1 << 20).expect("reopen");
        assert_eq!(decoded.sections(), &[a_key_map_section(), later]);
        // The second is a kind this build has no name for, and it survived the round trip anyway.
        // That is what keeps an old build from silently discarding a newer build's work when it
        // rewrites a directory.
        assert!(decoded.sections()[0].known());
        assert!(!decoded.sections()[1].known());
    }

    #[test]
    fn a_directory_written_before_the_section_table_reads_as_a_table_with_none() {
        // The G1 exit criterion, at the directory level. A format 22 directory is exactly this
        // build's directory with the trailing section block cut off, so cutting it off is the
        // honest way to make one: no fixture to go stale, and no separate encoder to drift.
        let directory = encode_directory(&bare_table(Vec::new())).expect("directory");
        let block = SECTIONS.len() + size_of::<u64>() + size_of::<u16>();
        let older = &directory[..directory.len() - block];
        let decoded = decode_directory(older, 1 << 20).expect("a directory from before sections");
        assert!(decoded.sections().is_empty());
        assert_eq!(decoded.generation(), 0, "a format 22 table recorded no generation");
        assert_eq!(decoded.name(), "linked");
        assert_eq!(decoded.fields().len(), 1, "everything before the block still decodes");
    }

    #[test]
    fn a_file_stamped_with_the_previous_format_still_opens_and_reads() {
        // The same criterion end to end, which is the one the milestone actually asks for: a build
        // that knows about sections opens a file written by a build that did not, with no rewrite
        // and no repair, and answers from it. The version field is patched rather than a file
        // committed by an old binary because the bytes either side of it are identical: format 22
        // and format 23 differ only in a trailing directory block, and a reader that stops before
        // that block gets a table with no sections.
        let path = path("format_twenty_two");
        let mut writer =
            Writer::create(&path, "items", vec![Field::required("id", LogicalType::Integer)])
                .expect("new file");
        let rows = Chunk::new(vec![
            Vector::from_values(
                LogicalType::Integer,
                &[Value::Integer(1), Value::Integer(2), Value::Integer(3)],
            )
            .expect("integers"),
        ])
        .expect("one column");
        writer.append(&rows).expect("the only part");
        writer.finish().expect("commit");

        let file = OpenOptions::new().write(true).open(&path).expect("reopen to patch");
        write_at(&file, 8, &22_u32.to_le_bytes()).expect("stamp the older format");
        drop(file);

        let reader = Reader::open(&path).expect("a format 22 file opens unchanged");
        assert_eq!(reader.table().rows(), 3);
        // The rows and not the section table, because the section block is found by the magic at
        // the end of the directory rather than by the number in the header, so stamping the header
        // back does not take away the summaries this writer put there. What the test is about is
        // that the version check accepts 22, and the rows coming back is what says it did.
        assert_eq!(reader.read(0, &[0]).expect("the part still reads").len(), 3);

        // And a format this build has never written is still refused, so the accept set is a list
        // and not an absence of a check.
        let file = OpenOptions::new().write(true).open(&path).expect("reopen to patch");
        write_at(&file, 8, &21_u32.to_le_bytes()).expect("stamp an unreadable format");
        drop(file);
        let error = Reader::open(&path).expect_err("format 21 is not readable");
        assert!(error.to_string().contains("format 21"), "{error}");

        fs::remove_file(&path).expect("clean up");
    }

    #[test]
    fn a_section_whose_extent_table_is_outside_the_file_is_refused() {
        // The bound the format has to check and `section` cannot, because only the reader knows how
        // big the file is. Reading the payload a section like this names would be reading whatever
        // else happens to be at that offset, which is the one way a graph section could turn into a
        // wrong answer rather than a slow one.
        let mut past = a_key_map_section();
        past.extent_page = 1 << 30;
        let directory = encode_directory(&bare_table(vec![past])).expect("directory");
        let error = decode_directory(&directory, 1 << 20).expect_err("refused");
        assert!(error.to_string().contains("outside the file"), "{error}");

        let mut inside_the_header = a_key_map_section();
        inside_the_header.extent_page = 8;
        let directory = encode_directory(&bare_table(vec![inside_the_header])).expect("directory");
        assert!(
            decode_directory(&directory, 1 << 20).is_err(),
            "a section may not overlap a header"
        );
    }

    #[test]
    fn a_section_recorded_as_not_built_is_legal_and_names_no_bytes() {
        // Section 3.7: a relationship that does not fit the budget is recorded with its size so
        // that `rudb_links()` can report what a larger budget would buy. That record is a section
        // entry with no extents, so it has to survive a round trip while naming nothing.
        let not_built = Section {
            kind: *section::FORWARD_LINK,
            id: 9,
            generation: 3,
            extents: 0,
            extent_page: 0,
            extent_bytes: 0,
            hash: 0,
            flags: 0,
            header_bytes: 0,
        };
        let directory = encode_directory(&bare_table(vec![not_built])).expect("directory");
        let decoded = decode_directory(&directory, 1 << 20).expect("reopen");
        assert_eq!(decoded.sections(), &[not_built]);

        // But a section with no extents that still names an extent table is incoherent, and an
        // incoherent entry is a torn directory rather than a relationship that was skipped.
        let mut incoherent = not_built;
        incoherent.extent_bytes = 28;
        incoherent.extent_page = HEADER;
        let directory = encode_directory(&bare_table(vec![incoherent])).expect("directory");
        assert!(decode_directory(&directory, 1 << 20).is_err());
    }

    #[test]
    fn a_directory_naming_more_sections_than_the_bound_is_refused() {
        let directory = encode_directory(&bare_table(Vec::new())).expect("directory");
        let mut torn = directory.clone();
        let count_at = torn.len() - size_of::<u16>();
        torn[count_at..].copy_from_slice(&u16::MAX.to_le_bytes());
        // Not an allocation of sixty five thousand entries off a torn count: either the bound
        // refuses it or the bytes run out, and both are errors rather than a read past the end.
        assert!(decode_directory(&torn, 1 << 20).is_err());
    }

    /// A committed one column file of `rows` integers, for the attach tests.
    fn linked_file(label: &str, rows: i32) -> PathBuf {
        let path = path(label);
        let mut writer =
            Writer::create(&path, "items", vec![Field::required("id", LogicalType::Integer)])
                .expect("new file");
        let values = (0..rows).map(Value::Integer).collect::<Vec<_>>();
        let chunk =
            Chunk::new(vec![Vector::from_values(LogicalType::Integer, &values).expect("integers")])
                .expect("one column");
        writer.append(&chunk).expect("the only part");
        writer.finish().expect("commit");
        path
    }

    fn a_key_map_payload() -> Vec<u8> {
        // Shaped like one without being one: this crate never reads a payload, so what matters here
        // is that every byte comes back and that the header the entry measures is at the front.
        (0..512_u32).flat_map(u32::to_le_bytes).collect()
    }

    #[test]
    fn a_section_attached_to_a_committed_file_reads_back_byte_for_byte() {
        let path = linked_file("attach", 64);
        let payload = a_key_map_payload();
        let table = attach(
            &path,
            "items",
            &[section::Attachment {
                kind: *section::KEY_MAP,
                id: 0,
                flags: 2,
                header_bytes: 40,
                bytes: &payload,
            }],
        )
        .expect("attach a key map");
        assert_eq!(attached(&table).len(), 1);

        let reader = Reader::open(&path).expect("reopen after the attach");
        let held = attached(reader.table());
        assert_eq!(held.len(), 1);
        assert_eq!(held[0].kind, *section::KEY_MAP);
        assert_eq!(held[0].flags, 2, "the form a reader must not have to guess");
        assert_eq!(held[0].header_bytes, 40);
        // The generation is the one the pages were written at, not the one the attach committed at.
        // Attaching a section moved no row, so a section written by it is current, and a second
        // table added to this file later would not make it stale.
        assert_eq!(held[0].generation, 1);
        assert!(held[0].usable(reader.table().generation()));
        assert_eq!(reader.payload(held[0]).expect("read the payload"), payload);
        assert_eq!(reader.extents(held[0]).expect("extent table").len(), 1);

        fs::remove_file(&path).expect("clean up");
    }

    #[test]
    fn attaching_a_section_answers_every_row_exactly_as_before() {
        // Section 3.1 end to end, and the reason the whole layer is safe to build incrementally. A
        // file with a section in it and the same file without one have to agree row for row, so the
        // comparison is made against the answers taken before the attach rather than against a
        // constant somebody typed.
        let path = linked_file("attach_changes_nothing", 300);
        let before = Reader::open(&path).expect("open before");
        let rows = before.table().rows();
        let first = before.read(0, &[0]).expect("read before");
        let values = (0..rows).map(|at| first.value_at(at, 0)).collect::<Vec<_>>();
        let layout = before.layout().columns_total();
        drop(before);

        let payload = a_key_map_payload();
        attach(
            &path,
            "items",
            &[section::Attachment {
                kind: *section::KEY_MAP,
                id: 0,
                flags: 0,
                header_bytes: 0,
                bytes: &payload,
            }],
        )
        .expect("attach");

        let after = Reader::open(&path).expect("open after");
        assert_eq!(after.table().rows(), rows);
        let read = after.read(0, &[0]).expect("read after");
        for (at, value) in values.iter().enumerate() {
            assert_eq!(&read.value_at(at, 0), value, "row {at} moved");
        }
        assert_eq!(
            after.layout().columns_total(),
            layout,
            "an attach appends and does not rewrite a column page"
        );

        fs::remove_file(&path).expect("clean up");
    }

    #[test]
    fn a_rebuilt_section_replaces_the_one_it_supersedes() {
        // Rebuilding a key map has to be a write and not a question. If an attach added rather than
        // replaced, a table rebuilt a few times would name several maps for one column and a reader
        // would have to pick, which is a decision with no right answer in it.
        let path = linked_file("attach_twice", 32);
        let one = a_key_map_payload();
        let two = vec![7_u8; 1024];
        let entry = |bytes| section::Attachment {
            kind: *section::KEY_MAP,
            id: 4,
            flags: 1,
            header_bytes: 0,
            bytes,
        };
        attach(&path, "items", &[entry(&one)]).expect("first build");
        attach(&path, "items", &[entry(&two)]).expect("rebuild");

        let reader = Reader::open(&path).expect("reopen");
        let held = attached(reader.table());
        assert_eq!(held.len(), 1, "one map per column and not one per build");
        assert_eq!(reader.payload(held[0]).expect("payload"), two);

        fs::remove_file(&path).expect("clean up");
    }

    #[test]
    fn an_attach_carries_through_a_kind_it_does_not_know() {
        // The first of section 3.2's three rules, at the point where it is easiest to break: a build
        // that rewrites a directory has to carry an entry it has no name for, or opening a file with
        // an older binary and attaching one section quietly deletes the work of a newer one.
        let path = linked_file("attach_unknown", 16);
        let payload = vec![3_u8; 96];
        attach(
            &path,
            "items",
            &[section::Attachment {
                kind: *b"RUDBZZ9\0",
                id: 1,
                flags: 0,
                header_bytes: 0,
                bytes: &payload,
            }],
        )
        .expect("a kind this build does not know still writes");
        let key_map = a_key_map_payload();
        attach(
            &path,
            "items",
            &[section::Attachment {
                kind: *section::KEY_MAP,
                id: 0,
                flags: 0,
                header_bytes: 0,
                bytes: &key_map,
            }],
        )
        .expect("attach beside it");

        let reader = Reader::open(&path).expect("reopen");
        let held = attached(reader.table());
        assert_eq!(held.len(), 2, "the unfamiliar entry survived a directory rewrite");
        let unknown = held.iter().find(|one| !one.known()).expect("the unfamiliar one");
        assert_eq!(reader.payload(unknown).expect("its bytes are still there"), payload);

        fs::remove_file(&path).expect("clean up");
    }

    #[test]
    fn a_payload_of_nothing_is_a_relationship_recorded_as_not_built() {
        let path = linked_file("attach_not_built", 8);
        attach(
            &path,
            "items",
            &[section::Attachment {
                kind: *section::FORWARD_LINK,
                id: 2,
                flags: 0,
                header_bytes: 0,
                bytes: &[],
            }],
        )
        .expect("record a link that did not fit the budget");

        let reader = Reader::open(&path).expect("reopen");
        let held = attached(reader.table());
        assert_eq!(held.len(), 1);
        assert_eq!(held[0].extents, 0);
        assert_eq!(held[0].extent_page, 0, "an entry that names no bytes points at none");
        assert!(reader.extents(held[0]).expect("no extent table").is_empty());
        assert!(reader.payload(held[0]).expect("no payload").is_empty());

        fs::remove_file(&path).expect("clean up");
    }

    #[test]
    fn a_payload_past_one_extent_is_split_and_joined_back() {
        // Issue #745's rule, exercised rather than argued. One byte past the bound is the smallest
        // payload that has to be two extents, and it is the case a split written for the common
        // size gets wrong.
        let path = linked_file("attach_two_extents", 8);
        let payload = vec![0x5a_u8; section::MAX_EXTENT as usize + 1];
        attach(
            &path,
            "items",
            &[section::Attachment {
                kind: *section::KEY_MAP,
                id: 0,
                flags: 0,
                header_bytes: 0,
                bytes: &payload,
            }],
        )
        .expect("attach a payload past the bound");

        let reader = Reader::open(&path).expect("reopen");
        let held = attached(reader.table());
        let extents = reader.extents(held[0]).expect("extent table");
        assert_eq!(extents.len(), 2, "one byte past the bound is two extents");
        assert_eq!(extents[0].length, section::MAX_EXTENT);
        assert_eq!(extents[1].length, 1);
        assert_eq!(extents[1].first, u64::from(section::MAX_EXTENT));
        // And the extent the caller wants is readable on its own, which is the point of the split.
        assert_eq!(reader.extent(&extents[1]).expect("the last extent"), vec![0x5a]);
        assert_eq!(reader.payload(held[0]).expect("the whole payload").len(), payload.len());

        fs::remove_file(&path).expect("clean up");
    }

    #[test]
    fn a_torn_extent_is_refused_rather_than_decoded() {
        let path = linked_file("attach_torn", 8);
        let payload = a_key_map_payload();
        attach(
            &path,
            "items",
            &[section::Attachment {
                kind: *section::KEY_MAP,
                id: 0,
                flags: 0,
                header_bytes: 0,
                bytes: &payload,
            }],
        )
        .expect("attach");

        let reader = Reader::open(&path).expect("reopen");
        let extent = reader.extents(&reader.table().sections()[0]).expect("extent table")[0];
        let file = OpenOptions::new().write(true).open(&path).expect("reopen to corrupt");
        write_at(&file, extent.offset + 7, &[0xff]).expect("flip a byte of the payload");
        drop(file);

        let reader = Reader::open(&path).expect("the table still opens");
        let error = reader
            .payload(&reader.table().sections()[0])
            .expect_err("a corrupt payload is not handed out");
        assert!(error.to_string().contains("checksum"), "{error}");
        // And the table is still readable, which is section 3.1: a section that cannot be trusted
        // costs the query its shortcut and nothing else.
        assert_eq!(reader.read(0, &[0]).expect("the column is untouched").width(), 1);

        fs::remove_file(&path).expect("clean up");
    }

    #[test]
    fn attaching_to_a_file_of_the_previous_format_is_refused_rather_than_done() {
        // Readable is not writable. A format 22 directory has no section block, and adding one
        // without moving the number in the header would leave a file claiming a format it is not.
        let path = linked_file("attach_old_format", 8);
        let file = OpenOptions::new().write(true).open(&path).expect("reopen to patch");
        write_at(&file, 8, &22_u32.to_le_bytes()).expect("stamp the older format");
        drop(file);

        let payload = a_key_map_payload();
        let error = attach(
            &path,
            "items",
            &[section::Attachment {
                kind: *section::KEY_MAP,
                id: 0,
                flags: 0,
                header_bytes: 0,
                bytes: &payload,
            }],
        )
        .expect_err("format 22 cannot gain a section");
        assert!(error.to_string().contains("format 22"), "{error}");
        assert!(Reader::open(&path).expect("and the file is untouched").table().rows() == 8);

        fs::remove_file(&path).expect("clean up");
    }

    #[test]
    fn a_section_header_longer_than_its_payload_is_refused_at_the_write() {
        let path = linked_file("attach_bad_header", 8);
        let error = attach(
            &path,
            "items",
            &[section::Attachment {
                kind: *section::KEY_MAP,
                id: 0,
                flags: 0,
                header_bytes: 40,
                bytes: &[1, 2, 3],
            }],
        )
        .expect_err("a writer's bug stops at the write");
        assert!(error.to_string().contains("header is longer"), "{error}");

        fs::remove_file(&path).expect("clean up");
    }

    #[test]
    fn attaching_to_a_name_the_file_does_not_hold_says_so() {
        let path = linked_file("attach_wrong_name", 8);
        let error = attach(&path, "orders", &[]).expect_err("no such table");
        assert!(error.to_string().contains("orders"), "{error}");
        fs::remove_file(&path).expect("clean up");
    }

    #[test]
    fn the_planner_gets_an_exact_count_for_a_leading_value_of_an_incomplete_synopsis() {
        // The case a complete synopsis does not cover, and the one worth the most. 16,000 rows over
        // 601 distinct values, 10,000 of them holding a single value and the rest spread ten apiece
        // over six hundred more. The writer holds 512 values, so the list is a prefix and most of
        // the tail is outside it. The counts inside it are still exact, because the pass recounts
        // the candidates that survived it, so `id = 1` is ten thousand rows rather than the
        // twenty six a distinct count of 601 would divide its way to.
        let path = path("frequency_prefix_for_the_planner");
        let mut writer =
            Writer::create(&path, "items", vec![Field::required("id", LogicalType::Integer)])
                .expect("new file");
        let mut values = vec![Value::Integer(1); 10_000];
        for _ in 0..10 {
            values.extend((0..600).map(|tail| Value::Integer(1_000 + tail)));
        }
        // A vector holds 8,192 rows, so this goes in as several parts. The pass that takes the
        // synopsis walks the whole column rather than a part, so the counts are the same either way.
        for part in values.chunks(8_000) {
            let rows = Chunk::new(vec![
                Vector::from_values(LogicalType::Integer, part).expect("integers"),
            ])
            .expect("one column");
            writer.append(&rows).expect("a part");
        }
        writer.finish().expect("commit");
        let reader = Reader::open(&path).expect("reopen from disk");
        let prefix =
            reader.frequency_prefix(0).expect("a readable synopsis").expect("the column has one");
        // A prefix and not the whole column, and the writer said how many rows anything left out of
        // it can hold.
        assert_eq!(prefix.entries.len(), 512);
        assert_eq!(prefix.omitted_max, 10);
        let common = Common::new(reader);
        assert_eq!(common.rows(), 16_000);
        let column = common.column("id").expect("the file has that column");
        assert_eq!(
            common.rows_with(column, &Bound::Int(1)),
            Stat::exact(10_000, Provenance::FrequencySynopsis)
        );
        // In the prefix, because ties go to the smaller value and the prefix reaches 1,510.
        assert_eq!(
            common.rows_with(column, &Bound::Int(1_100)),
            Stat::exact(10, Provenance::FrequencySynopsis)
        );
        // Outside it, and a prefix says nothing about a value it does not list. Not zero, which is
        // what a complete list would say, and the file holds ten rows of this one.
        assert_eq!(common.rows_with(column, &Bound::Int(1_550)), Stat::Unknown);
        // Not in the file at all, and still nothing rather than a zero. A prefix cannot tell the
        // two apart, which is the whole of what it gives up.
        assert_eq!(common.rows_with(column, &Bound::Int(9_999)), Stat::Unknown);
        // What the prefix left out, which is what turns the unknown above into a number. The 512
        // entries account for 15,110 rows, so 890 are left for the 89 values the writer dropped,
        // and 890 over 89 is the ten rows each of them really holds.
        let remainder = common.remainder(column).expect("the list is a prefix");
        assert_eq!(remainder, Remainder { rows: 890, listed: 512, most: 10 });
        assert_eq!(remainder.rows / (601 - remainder.listed), 10);
        fs::remove_file(&path).expect("clean up");
    }

    /// A file with no table in it is a file, and opening it says so rather than failing.
    #[test]
    fn a_file_holding_no_table_commits_and_opens_and_a_table_can_be_added_to_it() {
        let path = path("empty");
        Writer::empty(&path, &[]).expect("a file with nothing in it");
        let catalog = Catalog::open(&path).expect("the empty file opens");
        assert_eq!(catalog.len(), 0);
        assert!(catalog.is_empty());
        assert_eq!(catalog.names().count(), 0);
        // The next generation goes over the top of it the way it goes over any other, which is what
        // says this is a committed file and not a special case somebody has to know about.
        let mut writer =
            Writer::open(&path, "items", vec![Field::required("id", LogicalType::Integer)])
                .expect("a table goes into the empty file");
        writer.append(&sample_ids()).expect("rows");
        writer.finish().expect("commit");
        let catalog = Catalog::open(&path).expect("the file opens again");
        assert_eq!(catalog.names().collect::<Vec<_>>(), vec!["items"]);
        fs::remove_file(&path).expect("clean up");
    }

    /// A committed table with no rows is a name the next generation takes over, and one with rows
    /// is a name it refuses.
    ///
    /// The refusal is what it always was and it is load bearing: carrying a table that holds rows
    /// forward means reading and rewriting its pages, and a writer that quietly wrote a second
    /// entry under the same name would leave a file with two tables a reader cannot tell apart. An
    /// empty one has no pages and no reader, so there is nothing to carry and nothing to lose, and
    /// taking its place is what lets a schema committed by an earlier session be loaded by a stream
    /// instead of through memory.
    #[test]
    fn a_committed_empty_table_gives_up_its_name_and_one_with_rows_does_not() {
        let path = path("empty-name");
        let field = || vec![Field::required("id", LogicalType::Integer)];
        Writer::create(&path, "items", field()).expect("new file").finish().expect("commit");
        let catalog = Catalog::open(&path).expect("the file opens");
        assert_eq!(catalog.rows().collect::<Vec<_>>(), vec![("items", 0)]);

        let mut writer = Writer::open(&path, "items", field()).expect("the empty name is free");
        writer.append(&sample_ids()).expect("rows");
        writer.finish().expect("commit");
        let catalog = Catalog::open(&path).expect("the file opens again");
        // One entry and not two. The generation replaced the empty table rather than joining it.
        assert_eq!(catalog.names().collect::<Vec<_>>(), vec!["items"]);
        let held = catalog.rows().collect::<Vec<_>>();
        assert_eq!(held.len(), 1);
        assert!(held[0].1 > 0, "the rows that were appended are the ones the catalog counts");

        // The same call against the same name now that it holds rows, which is still refused.
        let error = Writer::open(&path, "items", field()).expect_err("a name with rows is taken");
        assert!(error.to_string().contains("same name"), "{error}");
        fs::remove_file(&path).expect("clean up");
    }

    /// A view, with everything about it that a reopened catalog has to be able to answer from.
    fn sample_view(name: &str) -> ViewEntry {
        ViewEntry {
            name: name.to_string(),
            sql: "SELECT id FROM items WHERE id > 0".to_string(),
            statement: format!("CREATE VIEW {name} AS SELECT id FROM items WHERE (id > 0);"),
            aliases: vec!["n".to_string()],
            columns: vec![Field::new("n", LogicalType::Integer)],
        }
    }

    #[test]
    fn a_view_written_into_the_catalog_comes_back_whole() {
        let path = path("views");
        let mut writer =
            Writer::create(&path, "items", vec![Field::required("id", LogicalType::Integer)])
                .expect("new file");
        writer.append(&sample_ids()).expect("rows");
        writer.with_views(vec![sample_view("v")]).finish().expect("commit");
        let catalog = Catalog::open(&path).expect("reopen");
        assert_eq!(catalog.views().cloned().collect::<Vec<_>>(), vec![sample_view("v")]);
        // The tables are still there and are still read the same way, so the section on the end did
        // not move anything in front of it.
        assert_eq!(catalog.names().collect::<Vec<_>>(), vec!["items"]);
        fs::remove_file(&path).expect("clean up");
    }

    /// A writer opened to append a table says nothing about views and must not lose them.
    #[test]
    fn appending_a_table_carries_the_views_forward() {
        let path = path("viewscarry");
        let mut writer =
            Writer::create(&path, "items", vec![Field::required("id", LogicalType::Integer)])
                .expect("new file");
        writer.append(&sample_ids()).expect("rows");
        writer.with_views(vec![sample_view("v")]).finish().expect("commit");
        let mut writer =
            Writer::open(&path, "other", vec![Field::required("id", LogicalType::Integer)])
                .expect("a second table");
        writer.append(&sample_ids()).expect("rows");
        writer.finish().expect("commit");
        let catalog = Catalog::open(&path).expect("reopen");
        assert_eq!(catalog.views().count(), 1);
        assert_eq!(catalog.names().collect::<Vec<_>>(), vec!["items", "other"]);
        fs::remove_file(&path).expect("clean up");
    }

    /// The whole point of [`Writer::restate`]: the views change and the pages do not move.
    #[test]
    fn restating_the_views_leaves_every_table_where_it_was() {
        let path = path("restate");
        let mut writer =
            Writer::create(&path, "items", vec![Field::required("id", LogicalType::Integer)])
                .expect("new file");
        writer.append(&sample_ids()).expect("rows");
        writer.finish().expect("commit");
        let before = fs::metadata(&path).expect("the file is there").len();
        Writer::restate(&path, &[sample_view("v"), sample_view("w")]).expect("two views");
        let catalog = Catalog::open(&path).expect("reopen");
        assert_eq!(catalog.views().count(), 2);
        assert_eq!(catalog.names().collect::<Vec<_>>(), vec!["items"]);
        // A catalog on the end and nothing else, so what it grew by is the size of a catalog rather
        // than the size of the table.
        let after = fs::metadata(&path).expect("the file is there").len();
        assert!(after > before, "a generation was written");
        assert!(after - before < before, "the table was not written again");
        // The rows are still readable through the new generation, which is the part that would go
        // wrong if the catalog carried the wrong directory pointers forward.
        let reader = Catalog::open(&path).expect("reopen").table("items").expect("the table");
        assert_eq!(reader.table().rows, 3);
        // And a restate over a restate keeps working, because each one reads the slot that
        // checksummed rather than the highest number in the header.
        Writer::restate(&path, &[]).expect("no views at all");
        assert_eq!(Catalog::open(&path).expect("reopen").views().count(), 0);
        fs::remove_file(&path).expect("clean up");
    }

    /// Two entries under one name is a catalog no lookup can answer, whichever two they are.
    #[test]
    fn a_view_named_after_a_table_is_refused_when_the_catalog_is_read() {
        let bytes = encode_catalog(
            &[Entry {
                name: "items".to_string(),
                fields: vec![Field::required("id", LogicalType::Integer)],
                rows: 1,
                directory: Page { offset: HEADER, length: 8, hash: 0 },
                nonzero: vec![None],
                aggregates: vec![None],
                distincts: vec![None],
                extremes: vec![None],
                frequencies: vec![None],
            }],
            &[sample_view("items")],
        )
        .expect("it encodes, because encoding does not look");
        let error = decode_catalog(&bytes, HEADER + 8).expect_err("and decoding does");
        assert!(error.to_string().contains("same name"), "{error}");
    }

    /// A compressed text page read at some rows is those rows of the page read whole, nulls and
    /// all, and a row past the end or rows out of order are refused rather than guessed at.
    #[test]
    fn a_compressed_text_page_read_at_some_rows_is_those_rows_of_the_whole() {
        let rows: usize = 300;
        let text: Vec<String> =
            (0..rows).map(|row| format!("a street named after number {}", row * 7)).collect();
        let values: Vec<&[u8]> = text.iter().map(String::as_bytes).collect();
        let mut page = vec![6, 2];
        page.extend((0..rows.div_ceil(8)).map(|byte| {
            (0..8).filter(|bit| (byte * 8 + bit) % 5 != 3).fold(0_u8, |mask, bit| mask | 1 << bit)
        }));
        let compressed = string::encode_only(string::Kind::Fsst, &values)
            .expect("encoded")
            .expect("text this repetitive compresses");
        page.extend_from_slice(&compressed);
        let whole = decode(&LogicalType::Varchar, rows, &page, None).expect("the whole page");
        let positions = [0_u32, 3, 8, 13, 200, 299];
        let some =
            decode_at(&LogicalType::Varchar, rows, &page, None, &positions).expect("some rows");
        assert_eq!(some.len(), positions.len());
        for (at, &row) in positions.iter().enumerate() {
            assert_eq!(some.value_at(at), whole.value_at(row as usize), "row {row}");
        }
        assert_eq!(some.value_at(1), Value::Null, "row 3 is null");
        assert!(decode_at(&LogicalType::Varchar, rows, &page, None, &[300]).is_err());
        assert!(decode_at(&LogicalType::Varchar, rows, &page, None, &[8, 3]).is_err());
    }

    /// Every column of a part read at some rows is the part read whole and gathered, whatever the
    /// page holds.
    #[test]
    fn a_part_read_at_some_rows_is_the_part_read_whole_and_gathered() {
        let path = path("rows");
        let mut writer = Writer::create(
            &path,
            "items",
            vec![
                Field::required("id", LogicalType::Integer),
                Field::new("text", LogicalType::Varchar),
            ],
        )
        .expect("new file");
        let rows = 2_000;
        let chunk = Chunk::new(vec![
            Vector::from_values(
                LogicalType::Integer,
                &(0..rows).map(Value::Integer).collect::<Vec<_>>(),
            )
            .expect("integers"),
            Vector::from_values(
                LogicalType::Varchar,
                &(0..rows)
                    .map(|row| {
                        if row % 7 == 2 {
                            Value::Null
                        } else {
                            Value::Varchar(format!("a comment about order {}", row * 13))
                        }
                    })
                    .collect::<Vec<_>>(),
            )
            .expect("strings"),
        ])
        .expect("matching rows");
        writer.append(&chunk).expect("one part");
        writer.finish().expect("commit");
        let reader = Reader::open(&path).expect("reopen from disk");
        let positions = [1_u32, 2, 9, 1_000, 1_999];
        for whole in [true, false] {
            let some = reader.read_rows(0, &[0, 1], &positions, whole).expect("some rows");
            let all = reader.read(0, &[0, 1]).expect("the whole part");
            assert_eq!(some.len(), positions.len());
            for column in 0..2 {
                for (at, &row) in positions.iter().enumerate() {
                    assert_eq!(some.value_at(at, column), all.value_at(row as usize, column));
                }
            }
        }
        assert!(reader.read_rows(0, &[1], &[2_000], true).is_err());
    }

    #[test]
    fn committed_file_reopens_and_reads_only_requested_columns() {
        let path = path("reopen");
        let mut writer = Writer::create(
            &path,
            "items",
            vec![
                Field::required("id", LogicalType::Integer),
                Field::new("text", LogicalType::Varchar),
            ],
        )
        .expect("new file");
        writer.append(&sample()).expect("first part");
        writer.append(&sample()).expect("second part");
        writer.finish().expect("commit");
        let reader = Reader::open(&path).expect("reopen from disk");
        assert_eq!(reader.table().rows(), 6);
        // Two appends below the stripe bound are two parts of one stripe, which is the whole point
        // of the split: the directory describes the stripe and the scan still reads a part.
        assert_eq!(reader.table().stripes().len(), 1);
        assert_eq!(reader.parts(), 2);
        assert_eq!(reader.part_rows(0), 3);
        assert_eq!(reader.part_rows(1), 3);
        let text = reader.read(1, &[1]).expect("only text page");
        assert_eq!(text.width(), 1);
        assert_eq!(text.value_at(1, 0), Value::Null);
        assert_eq!(text.value_at(2, 0), Value::Varchar("long text after a slash".into()));
        let sparse = reader.read_sparse(1, &[1]).expect("one part without its whole page");
        assert_eq!(sparse.width(), 1);
        assert_eq!(sparse.value_at(1, 0), Value::Null);
        assert_eq!(sparse.value_at(2, 0), Value::Varchar("long text after a slash".into()));
        assert!(!reader.skips_codes(0, 1, &[0]).expect("alpha is in the stripe"));
        assert!(!reader.skips_codes(0, 1, &[2]).expect("long text is in the stripe"));
        assert!(reader.skips_codes(0, 1, &[3]).expect("unknown code is absent"));
        let count = reader.read(0, &[]).expect("no page is needed for count");
        assert_eq!(count.len(), 3);
        assert!(reader.skips(0, &[Probe { column: 0, op: Op::Greater, value: Bound::Int(100) }]));
        assert!(!reader.skips(0, &[Probe { column: 0, op: Op::Greater, value: Bound::Int(0) }]));
        assert_eq!(reader.top_frequencies(0, 1).expect("valid integer synopsis"), None);
        let integers = reader.frequency_prefix(0).expect("valid integer synopsis").expect("kept");
        assert_eq!(integers.entries, vec![(Value::Integer(-2), 2), (Value::Integer(4), 2)]);
        assert_eq!(integers.omitted_max, 2);
        let strings = reader.top_frequencies(1, 1).expect("valid string synopsis").expect("kept");
        assert_eq!(strings.len(), 3);
        assert!(strings.contains(&(Value::Null, 2)));
        assert!(strings.contains(&(Value::Varchar("alpha".into()), 2)));
        assert!(strings.contains(&(Value::Varchar("long text after a slash".into()), 2)));
        fs::remove_file(path).expect("remove scratch file");
    }

    /// Two pipeline instances handing over whole runs, which is what makes the native sink safe to
    /// instance.
    ///
    /// The runs arrive in the order the instances finished reading them rather than in source
    /// order, and the second one to finish is the one that read the earlier rows. Each run is still
    /// a stripe of its own and the table still reads back in source order, which is the whole of
    /// what the writer promises about ordering.
    #[test]
    fn runs_handed_over_out_of_order_still_read_back_in_source_order() {
        let path = path("interleaved-runs");
        let mut writer =
            Writer::create(&path, "interleaved", vec![Field::new("v", LogicalType::BigInt)])
                .expect("new file");
        for morsel in [2_u64, 0, 3, 1] {
            let parts = (0..4_u64)
                .map(|chunk| {
                    let first = i64::try_from(morsel * 32 + chunk * 8).expect("small");
                    let values =
                        (0..8_i64).map(|row| Value::BigInt(first + row)).collect::<Vec<_>>();
                    let column =
                        Vector::from_values(LogicalType::BigInt, &values).expect("a column");
                    ((morsel, chunk), Chunk::new(vec![column]).expect("one column"))
                })
                .collect::<Vec<_>>();
            writer.append_stripe(parts).expect("a stripe");
        }
        writer.finish().expect("commit");

        let reader = Reader::open(&path).expect("valid directory");
        assert_eq!(reader.table().stripes().len(), 4, "a run is a stripe of its own");
        assert_eq!(reader.table().rows(), 128);
        for part in 0..16_usize {
            let read = reader.read(part, &[0]).expect("a part back");
            for row in 0..8_usize {
                let want = i64::try_from(part * 8 + row).expect("small");
                assert_eq!(read.value_at(row, 0), Value::BigInt(want), "part {part} row {row}");
            }
        }
        fs::remove_file(path).expect("remove scratch file");
    }

    /// Runs from different callers may interleave and may not overlap, and the commit is what
    /// catches an overlap.
    #[test]
    fn runs_that_overlap_each_other_are_refused_at_commit() {
        let path = path("overlapping-runs");
        let mut writer =
            Writer::create(&path, "overlapping", vec![Field::new("v", LogicalType::BigInt)])
                .expect("new file");
        let one = |order: (u64, u64)| {
            let column =
                Vector::from_values(LogicalType::BigInt, &[Value::BigInt(1)]).expect("a column");
            (order, Chunk::new(vec![column]).expect("one column"))
        };
        // The second run sits inside the first rather than after it, which is a thing no instance
        // holding its own contiguous run can produce and a thing the file cannot represent.
        writer.append_stripe(vec![one((0, 0)), one((0, 2))]).expect("a stripe");
        writer.append_stripe(vec![one((0, 1))]).expect("a stripe");
        let error = writer.finish().expect_err("the runs overlap");
        assert!(error.message().contains("source order"), "{error}");
        fs::remove_file(path).expect("remove scratch file");
    }

    /// A stripe holds [`STRIPE_PARTS`] parts, so a run longer than that is a caller bug rather than
    /// something to split, and the writer says so at the door instead of quietly cutting it in two.
    #[test]
    fn a_run_longer_than_a_stripe_is_refused() {
        let path = path("overlong-run");
        let mut writer =
            Writer::create(&path, "overlong", vec![Field::new("v", LogicalType::BigInt)])
                .expect("new file");
        let parts = (0..=STRIPE_PARTS)
            .map(|at| {
                let column = Vector::from_values(LogicalType::BigInt, &[Value::BigInt(1)])
                    .expect("a column");
                let chunk = Chunk::new(vec![column]).expect("one column");
                ((0, u64::try_from(at).expect("small")), chunk)
            })
            .collect::<Vec<_>>();
        let error = writer.append_stripe(parts).expect_err("one part too many");
        assert!(error.message().contains("more parts than it holds"), "{error}");
        fs::remove_file(path).expect("remove scratch file");
    }

    /// Parts past the stripe bound start a new stripe, and every part stays addressable on its own.
    ///
    /// This is the shape the format exists for, so both ends of the split are checked here. The
    /// directory holds three stripes rather than a hundred and thirty one, and a read of any one
    /// part still answers with that part's rows rather than with its whole stripe's.
    #[test]
    fn parts_past_the_stripe_bound_start_a_new_stripe() {
        let path = path("stripe-bound");
        let mut writer = Writer::create(
            &path,
            "items",
            vec![
                Field::required("id", LogicalType::Integer),
                Field::new("text", LogicalType::Varchar),
            ],
        )
        .expect("new file");
        let parts = STRIPE_PARTS * 2 + 3;
        for part in 0..parts {
            let id = part as i32;
            let chunk = Chunk::new(vec![
                Vector::from_values(
                    LogicalType::Integer,
                    &[Value::Integer(id), Value::Integer(-id)],
                )
                .expect("integers"),
                Vector::from_values(
                    LogicalType::Varchar,
                    &[Value::Varchar(format!("value {part}")), Value::Null],
                )
                .expect("strings"),
            ])
            .expect("matching rows");
            writer.append(&chunk).expect("one part");
        }
        writer.finish().expect("commit");

        let reader = Reader::open(&path).expect("reopen from disk");
        assert_eq!(reader.parts(), parts);
        assert_eq!(reader.table().rows(), parts * 2);
        assert_eq!(reader.table().stripes().len(), parts.div_ceil(STRIPE_PARTS));
        assert_eq!(reader.table().stripes()[0].parts(), STRIPE_PARTS);
        assert_eq!(reader.table().stripes()[0].rows(), STRIPE_PARTS * 2);
        assert_eq!(reader.table().stripes()[2].parts(), 3);
        // Backwards on purpose. The reader keeps four stripes a column, so a scan that walks the
        // table the other way is what catches a cache that only ever holds what it just read.
        for part in (0..parts).rev() {
            let dense = reader.read(part, &[0, 1]).expect("a whole page read");
            let sparse = reader.read_sparse(part, &[0, 1]).expect("one part read");
            for chunk in [&dense, &sparse] {
                assert_eq!(chunk.len(), 2, "part {part} has its own row count");
                assert_eq!(chunk.value_at(0, 0), Value::Integer(part as i32));
                assert_eq!(chunk.value_at(1, 0), Value::Integer(-(part as i32)));
                assert_eq!(chunk.value_at(0, 1), Value::Varchar(format!("value {part}")));
                assert_eq!(chunk.value_at(1, 1), Value::Null);
            }
        }
        // The bounds are merged over the stripe, so they answer for the range the whole stripe
        // covers and not for the part that was asked about.
        let above = [Probe { column: 0, op: Op::Greater, value: Bound::Int(100) }];
        assert!(reader.skips(0, &above), "the first stripe stops at 63");
        assert!(!reader.skips(STRIPE_PARTS * 2, &above), "the third stripe reaches 130");
        fs::remove_file(path).expect("remove scratch file");
    }

    /// A scattered value in the column that decides `WHERE UserID = ?`.
    fn scattered(n: i64) -> i64 {
        n.wrapping_mul(-7_046_029_254_386_353_131)
    }

    /// A part whose sieve does not hold the constant is skipped, and a range would skip none of them.
    ///
    /// This is ClickBench query 19 in miniature. The values are spread over the whole of `BIGINT`, so
    /// every stripe's bounds cover nearly all of it and rule out nothing, and the part that really
    /// holds the value is the only one a scan has to read.
    #[test]
    fn a_part_is_skipped_when_its_sieve_does_not_hold_the_constant() {
        let path = path("sieve-skip");
        let mut writer =
            Writer::create(&path, "hits", vec![Field::required("id", LogicalType::BigInt)])
                .expect("new file");
        let parts = STRIPE_PARTS + 3;
        // Big enough that the filter is worth its bytes. A part of eight numbers packs to under a
        // hundred bytes and the smallest filter there is is sixty nine, so a filter over a part
        // that small costs about as much to read as the rows do and is no longer written.
        let per_part = 128;
        for part in 0..parts {
            let held: Vec<Value> = (0..per_part)
                .map(|row| Value::BigInt(scattered((part * per_part + row) as i64)))
                .collect();
            let chunk =
                Chunk::new(vec![Vector::from_values(LogicalType::BigInt, &held).expect("numbers")])
                    .expect("one column");
            writer.append(&chunk).expect("one part");
        }
        writer.finish().expect("commit");

        let reader = Reader::open(&path).expect("reopen from disk");
        let probe = |value: i64| Probe {
            column: 0,
            op: Op::Equal,
            value: Bound::Int(i128::from(scattered(value))),
        };
        for wanted in [0_i64, (per_part + 1) as i64, (parts * per_part - 1) as i64] {
            let tests = [probe(wanted)];
            let kept: Vec<usize> = (0..parts).filter(|&part| !reader.skips(part, &tests)).collect();
            let home = wanted as usize / per_part;
            assert!(kept.contains(&home), "the part holding {wanted} is read");
            // A filter answers maybe, so a part it keeps need not hold the value. Sixty seven parts
            // of a hundred and twenty eight numbers each, at a dozen bits a value, is about one
            // stray part across the whole file and that is what this leaves room for.
            assert!(kept.len() <= 2, "{wanted} keeps {kept:?}, which is more than one stray part");
        }
        let absent = [probe((parts * per_part) as i64 + 1)];
        let kept = (0..parts).filter(|&part| !reader.skips(part, &absent)).count();
        assert!(kept <= 1, "{kept} parts of {parts} kept a value no part holds");
        // The same probes against the bounds alone, which is what this replaces. A column of
        // scattered numbers has a range per stripe that covers nearly the whole type.
        let tests = [probe(0)];
        assert!(
            reader.table().stripes().iter().all(|stripe| !stripe.zone.skips(&tests)),
            "the bounds rule out no stripe at all"
        );
        fs::remove_file(path).expect("remove scratch file");
    }

    /// A part whose own bounds rule out an ordered comparison is skipped where the stripe's keep it.
    ///
    /// This is the shape of ClickBench 24. Each part covers a narrow stretch of the column and the
    /// stripe covers all sixty four of them at once, so a comparison that lands inside the stripe
    /// rules out none of it and rules out all but a few parts.
    #[test]
    fn a_part_is_skipped_when_its_own_bounds_rule_out_a_comparison_the_stripe_keeps() {
        let path = path("part-range-skip");
        let mut writer =
            Writer::create(&path, "hits", vec![Field::required("at", LogicalType::BigInt)])
                .expect("new file");
        let parts = STRIPE_PARTS + 3;
        let per_part = 128;
        for part in 0..parts {
            // Scattered inside the part's own band rather than a run, because a run of
            // consecutive numbers encodes to a stride of a few bytes and then the page of ranges
            // costs more than reading the column it indexes, which is the case the writer declines.
            let held: Vec<Value> = (0..per_part)
                .map(|row| {
                    Value::BigInt((part * 1_000) as i64 + (scattered(row as i64).rem_euclid(900)))
                })
                .collect();
            let chunk =
                Chunk::new(vec![Vector::from_values(LogicalType::BigInt, &held).expect("numbers")])
                    .expect("one column");
            writer.append(&chunk).expect("one part");
        }
        writer.finish().expect("commit");

        let reader = Reader::open(&path).expect("reopen from disk");
        let under = [Probe { column: 0, op: Op::Less, value: Bound::Int(3_000) }];
        let kept: Vec<usize> = (0..parts).filter(|&part| !reader.skips(part, &under)).collect();
        assert_eq!(kept, vec![0, 1, 2], "only the three parts that start under three thousand");
        // The same question asked of the stripe alone, which is what this replaces.
        assert!(!reader.stripe_skips(0, &under), "the stripe reaches from zero and keeps itself");
        fs::remove_file(path).expect("remove scratch file");
    }

    /// The other half of the same page. A part whose own bounds put every row of it inside the
    /// filter is waved through, so the comparison never runs on it, where the stripe's bounds reach
    /// across every part and can prove nothing.
    #[test]
    fn a_part_is_waved_through_when_its_own_bounds_pass_a_comparison_the_stripe_cannot() {
        let path = path("part-range-certain");
        let mut writer =
            Writer::create(&path, "hits", vec![Field::required("at", LogicalType::BigInt)])
                .expect("new file");
        let parts = STRIPE_PARTS + 3;
        let per_part = 128;
        for part in 0..parts {
            let held: Vec<Value> = (0..per_part)
                .map(|row| {
                    Value::BigInt((part * 1_000) as i64 + (scattered(row as i64).rem_euclid(900)))
                })
                .collect();
            let chunk =
                Chunk::new(vec![Vector::from_values(LogicalType::BigInt, &held).expect("numbers")])
                    .expect("one column");
            writer.append(&chunk).expect("one part");
        }
        writer.finish().expect("commit");

        let reader = Reader::open(&path).expect("reopen from disk");
        let under = [Probe { column: 0, op: Op::Less, value: Bound::Int(3_000) }];
        let waved: Vec<usize> = (0..parts).filter(|&part| reader.certain(part, &under)).collect();
        assert_eq!(waved, vec![0, 1, 2], "the three parts that end under three thousand");
        // The first stripe reaches from zero to past sixty thousand, so it straddles three thousand
        // and settles nothing either way. The three yeses above are the parts' own ends talking.
        assert!(!reader.stripe_skips(0, &under), "the stripe straddles the comparison");
        fs::remove_file(path).expect("remove scratch file");
    }

    /// The page is worth its bytes on a column with parts to tell apart and is not written on one
    /// that has a single part, where the stripe bounds already are the part's.
    #[test]
    fn a_stripe_of_one_part_writes_no_range_page_and_a_stripe_of_many_does() {
        for (parts, wanted) in [(1_usize, false), (STRIPE_PARTS, true)] {
            let path = path("part-range-page");
            let mut writer =
                Writer::create(&path, "hits", vec![Field::required("at", LogicalType::BigInt)])
                    .expect("new file");
            for part in 0..parts {
                let held: Vec<Value> = (0..128)
                    .map(|row| {
                        Value::BigInt((part * 1_000) as i64 + scattered(row as i64).rem_euclid(900))
                    })
                    .collect();
                let chunk = Chunk::new(vec![
                    Vector::from_values(LogicalType::BigInt, &held).expect("numbers"),
                ])
                .expect("one column");
                writer.append(&chunk).expect("one part");
            }
            writer.finish().expect("commit");
            let reader = Reader::open(&path).expect("reopen from disk");
            let bytes = reader.layout().columns[0].part_ranges;
            assert_eq!(bytes > 0, wanted, "{parts} parts wrote {bytes} bytes of ranges");
            fs::remove_file(path).expect("remove scratch file");
        }
    }

    /// A cut down string end is still an end on the side it was, which is the only thing that keeps
    /// a shortened bound from turning a skip into a wrong answer.
    #[test]
    fn a_string_end_that_is_cut_down_still_covers_the_value_it_came_from() {
        let long = vec![b'a'; PART_BOUND_BYTES * 2];
        let low = shortened(Some(Bound::Bytes(long.clone())), false).expect("a low end");
        let high = shortened(Some(Bound::Bytes(long.clone())), true).expect("a high end");
        let Bound::Bytes(low) = low else { panic!("a string stays a string") };
        let Bound::Bytes(high) = high else { panic!("a string stays a string") };
        assert!(low.len() <= PART_BOUND_BYTES && high.len() <= PART_BOUND_BYTES);
        assert!(low.as_slice() <= long.as_slice(), "the low end is at or under the value");
        assert!(high.as_slice() >= long.as_slice(), "the high end is at or over the value");
    }

    /// A string of nothing but the largest byte has no prefix that can be stepped up, so the high
    /// end is given up rather than claimed too small. No end keeps the part, which is always safe.
    #[test]
    fn a_string_end_with_no_room_to_step_up_gives_up_the_bound() {
        let long = vec![u8::MAX; PART_BOUND_BYTES * 2];
        assert_eq!(shortened(Some(Bound::Bytes(long.clone())), true), None);
        let low = shortened(Some(Bound::Bytes(long)), false).expect("a low end is still a prefix");
        assert_eq!(low, Bound::Bytes(vec![u8::MAX; PART_BOUND_BYTES]));
    }

    /// What a column is stored as, asked of two files holding the same rows in a different order.
    ///
    /// This is the question the report exists to answer and it is the one the directory cannot. The
    /// two files have the same rows, the same schema and the same number of parts, and the column
    /// comes out four times smaller in one of them, because ascending keys delta encode to a few
    /// bits a row and shuffled ones do not. Nothing about the file's shape says so. The page header
    /// says so, and reading it is what this does.
    ///
    /// It is q18 on TPC-H in miniature: clustering lineitem by ship date leaves `l_orderkey`
    /// ascending inside a partition but sparse, its deltas go from six bits to twelve, and the scan
    /// pays for the wider ones.
    #[test]
    fn what_a_column_is_stored_as_follows_the_order_the_rows_were_written_in() {
        let parts = 4;
        let per_part = 1024;
        let rows = parts * per_part;
        let written = |name: &str, keys: &[i64]| {
            let path = path(name);
            let fields = vec![Field::required("key", LogicalType::BigInt)];
            let mut writer = Writer::create(&path, "keys", fields).expect("new file");
            for part in 0..parts {
                let values: Vec<Value> = keys[part * per_part..(part + 1) * per_part]
                    .iter()
                    .map(|key| Value::BigInt(*key))
                    .collect();
                let chunk = Chunk::new(vec![
                    Vector::from_values(LogicalType::BigInt, &values).expect("numbers"),
                ])
                .expect("one column");
                writer.append(&chunk).expect("one part");
            }
            writer.finish().expect("commit");
            path
        };
        // Ascending with a small irregular step, which is what a key column in arrival order looks
        // like: an order has one to seven line items, so the key repeats and then moves on by one.
        let climbing = |step: &dyn Fn(usize) -> i64| {
            let mut key = 0;
            (0..rows)
                .map(|row| {
                    key += step(row);
                    key
                })
                .collect::<Vec<i64>>()
        };
        let ascending = climbing(&|row| (row % 3) as i64);
        // The same rows in the same direction over a range a thousand times wider, which is what a
        // partition of a clustered table holds: still ascending, and far enough apart that the
        // deltas no longer fit in a handful of bits.
        let sparse = climbing(&|row| ((row * 2_654_435_761) % 4096) as i64);
        let near_path = written("stored-near", &ascending);
        let far_path = written("stored-far", &sparse);

        let one = Reader::open(&near_path).expect("reopen from disk");
        let other = Reader::open(&far_path).expect("reopen from disk");
        let near = one.stored(0).expect("the column is stored");
        let far = other.stored(0).expect("the column is stored");
        assert_eq!(near.len(), parts, "one row per part");
        assert_eq!(far.len(), parts);
        // The bytes are the same bytes the directory totals, which is the check that this is
        // reading the pages the file really holds rather than some other pages.
        let total = |stored: &[StoredPart]| stored.iter().map(|part| part.bytes).sum::<u64>();
        assert_eq!(total(&near), one.layout().columns[0].pages);
        assert_eq!(total(&far), other.layout().columns[0].pages);
        assert!(
            total(&near) * 2 < total(&far),
            "the sparse keys cost more, {} against {}",
            total(&far),
            total(&near)
        );
        // Every part accounted for, in order, with the row it starts at following the one before.
        for (at, part) in near.iter().enumerate() {
            assert_eq!(part.part, at);
            assert_eq!(part.row, at * per_part);
            assert_eq!(part.rows, per_part);
            let held = &ascending[at * per_part..(at + 1) * per_part];
            assert_eq!(part.low, Some(Value::BigInt(held[0])));
            assert_eq!(part.high, Some(Value::BigInt(held[per_part - 1])));
            assert_eq!(part.nulls, Some(0));
        }
        // And the encoding is a line of text that names what the encoder chose, which is the whole
        // point. Both are a cascade over deltas and the widths inside them are what differ.
        assert!(near[0].encoding.contains("DELTA"), "{}", near[0].encoding);
        assert!(far[0].encoding.contains("DELTA"), "{}", far[0].encoding);
        assert_ne!(near[0].encoding, far[0].encoding);
        fs::remove_file(near_path).expect("remove scratch file");
        fs::remove_file(far_path).expect("remove scratch file");
    }

    /// A sieve bigger than the part it indexes is not written, and one smaller than it still is.
    ///
    /// Both columns hold values spread over the whole of `BIGINT`, so neither gets a bitmap and both
    /// reach the filter. They differ in what the part costs to read. `spread` is a thousand distinct
    /// numbers and packs to eight kilobytes, so a filter of about thirteen hundred bytes is a good
    /// trade. `repeated` is the same thousand rows over four numbers in runs and encodes to
    /// almost nothing, but the filter is sized for the rows rather than the values it turns out to
    /// hold, so it comes out larger than the data. Reading it to decide whether to read the part spends more than
    /// the part, every time, and that is the case this drops.
    #[test]
    fn a_sieve_larger_than_the_part_it_indexes_is_not_written() {
        let path = path("sieve-pays");
        let fields = vec![
            Field::required("spread", LogicalType::BigInt),
            Field::required("repeated", LogicalType::BigInt),
        ];
        let mut writer = Writer::create(&path, "hits", fields).expect("new file");
        let parts = 3;
        let per_part = 1024;
        for part in 0..parts {
            let base = (part * per_part) as i64;
            let spread: Vec<Value> =
                (0..per_part).map(|row| Value::BigInt(scattered(base + row as i64))).collect();
            let repeated: Vec<Value> =
                (0..per_part).map(|row| Value::BigInt(scattered((row / 256) as i64))).collect();
            let chunk = Chunk::new(vec![
                Vector::from_values(LogicalType::BigInt, &spread).expect("numbers"),
                Vector::from_values(LogicalType::BigInt, &repeated).expect("numbers"),
            ])
            .expect("two columns");
            writer.append(&chunk).expect("one part");
        }
        writer.finish().expect("commit");

        let reader = Reader::open(&path).expect("reopen from disk");
        let layout = reader.layout();
        let spread = &layout.columns[0];
        let repeated = &layout.columns[1];
        assert!(spread.sieves > 0, "a column whose parts are worth a filter keeps one");
        assert_eq!(
            repeated.sieves, 0,
            "a column whose filter costs more than its parts keeps none"
        );
        // Per part this is the rule itself, so it holds over the column as well: a part without a
        // sieve adds to one side of this and to nothing on the other.
        for column in &layout.columns {
            assert!(
                column.sieves < column.pages,
                "{} spends {} on sieves over {} of data",
                column.name,
                column.sieves,
                column.pages
            );
        }
        // The filter that was kept still does what it is for.
        let absent = [Probe {
            column: 0,
            op: Op::Equal,
            value: Bound::Int(i128::from(scattered((parts * per_part) as i64 + 1))),
        }];
        assert!((0..parts).all(|part| reader.skips(part, &absent)), "no part holds it");
        fs::remove_file(path).expect("remove scratch file");
    }

    /// A damaged sieve page is a part that gets read, not a query that fails.
    ///
    /// A sieve is an index over rows that are still there and still correct, so losing one costs
    /// time and costs no answers. That is the opposite of the membership index beside it, which is
    /// the only thing standing between a string page and a wrong answer.
    #[test]
    fn a_damaged_sieve_page_is_read_through_rather_than_refused() {
        let path = path("sieve-damaged");
        let mut writer =
            Writer::create(&path, "hits", vec![Field::required("id", LogicalType::BigInt)])
                .expect("new file");
        let rows = 128;
        let held: Vec<Value> = (0..rows).map(|row| Value::BigInt(scattered(row))).collect();
        let chunk =
            Chunk::new(vec![Vector::from_values(LogicalType::BigInt, &held).expect("numbers")])
                .expect("one column");
        writer.append(&chunk).expect("one part");
        writer.finish().expect("commit");

        let page = Reader::open(&path).expect("reopen").table.stripes[0]
            .sieves
            .get(0)
            .expect("a sieve page");
        let mut file = OpenOptions::new().write(true).open(&path).expect("open the sieve page");
        file.seek(SeekFrom::Start(page.offset + u64::from(page.length) - 1)).expect("seek");
        file.write_all(&[0xff]).expect("damage one byte");
        drop(file);

        let reader = Reader::open(&path).expect("reopen the damaged file");
        let absent =
            [Probe { column: 0, op: Op::Equal, value: Bound::Int(i128::from(scattered(99))) }];
        assert!(!reader.skips(0, &absent), "a sieve that cannot be read skips nothing");
        assert_eq!(
            reader.read(0, &[0]).expect("the rows are untouched").len(),
            usize::try_from(rows).expect("a small count")
        );
        fs::remove_file(path).expect("remove scratch file");
    }

    /// Eight workers over one stripe read it once between them.
    ///
    /// This is the shape a scan actually has. Parts are handed out in order, so every worker on a
    /// column crosses into a stripe within a few parts of the others, and before [`Reader::held`]
    /// started sharing the read every one of them read the whole page. On the full ClickBench file
    /// that was a `MIN(EventDate), MAX(EventDate)` moving 3.2 GB off the disk to look at 400 MB of
    /// column, which is most of what a first touch costs.
    ///
    /// The workers that lose the race still answer, out of the part reads they do instead, which is
    /// what the values below are checking.
    #[test]
    fn workers_that_want_the_same_stripe_read_it_once() {
        let path = path("single-flight");
        let mut writer =
            Writer::create(&path, "items", vec![Field::required("id", LogicalType::Integer)])
                .expect("new file");
        for part in 0..STRIPE_PARTS {
            let id = part as i32;
            let chunk = Chunk::new(vec![
                Vector::from_values(
                    LogicalType::Integer,
                    &[Value::Integer(id), Value::Integer(-id)],
                )
                .expect("integers"),
            ])
            .expect("matching rows");
            writer.append(&chunk).expect("one part");
        }
        writer.finish().expect("commit");

        let reader = Reader::open(&path).expect("reopen from disk");
        assert_eq!(reader.table().stripes().len(), 1, "one stripe is the point of the test");
        let barrier = std::sync::Barrier::new(8);
        std::thread::scope(|scope| {
            for worker in 0..8 {
                let reader = &reader;
                let barrier = &barrier;
                scope.spawn(move || {
                    barrier.wait();
                    for part in (worker..STRIPE_PARTS).step_by(8) {
                        let chunk = reader.read(part, &[0]).expect("a whole page read");
                        assert_eq!(chunk.value_at(0, 0), Value::Integer(part as i32));
                        assert_eq!(chunk.value_at(1, 0), Value::Integer(-(part as i32)));
                    }
                });
            }
        });
        assert_eq!(reader.pages.load(Atomic::Relaxed), 1, "one stripe, one page read, whoever won");
        fs::remove_file(path).expect("remove scratch file");
    }

    /// Opening a file reads the header and the directory, and nothing that depends on the rows.
    ///
    /// `spec/stats/04-in-memory.md` section 4.2. There are no statistics in the file yet, so this
    /// holds today by not having anything to load, and that is exactly why it is worth pinning now.
    /// The change that breaks it is the reasonable looking one: summaries are a few hundred bytes,
    /// the next query will want them, so read them on the way past. A process that opened the
    /// database to run one trivial query pays for all of it and gets nothing.
    ///
    /// Two files of the same shape and a thousand times the rows in one of them, opened, and the
    /// two openings cost the same. The stripe count is held equal so that the directory is the same
    /// size in both, which leaves the rows as the only thing that changed. Anything read out of the
    /// data would show up here.
    #[test]
    fn opening_costs_the_same_over_a_thousand_times_the_rows() {
        let opened = |label: &str, rows_per_part: i32| {
            let path = path(label);
            let mut writer =
                Writer::create(&path, "items", vec![Field::required("id", LogicalType::Integer)])
                    .expect("new file");
            for part in 0..STRIPE_PARTS * 3 {
                // Scrambled rather than sequential, so that the fat file is actually fatter. A run
                // of consecutive integers encodes to almost nothing and would leave the two files
                // the same size, which would make this test pass for the wrong reason.
                let values = (0..rows_per_part)
                    .map(|row| {
                        Value::Integer((part as i32 * rows_per_part + row).wrapping_mul(2_654_435))
                    })
                    .collect::<Vec<_>>();
                let chunk = Chunk::new(vec![
                    Vector::from_values(LogicalType::Integer, &values).expect("integers"),
                ])
                .expect("matching rows");
                writer.append(&chunk).expect("one part");
            }
            writer.finish().expect("commit");
            let reader = Reader::open(&path).expect("reopen from disk");
            let size = fs::metadata(&path).expect("the file is there").len();
            let out = (reader.reads(), reader.table().stripes().len(), size);
            fs::remove_file(path).expect("remove scratch file");
            out
        };

        let (thin, thin_stripes, thin_size) = opened("open-thin", 1);
        let (fat, fat_stripes, fat_size) = opened("open-fat", 1000);
        assert_eq!(
            thin_stripes, fat_stripes,
            "the same stripe count is what makes this a fair ask"
        );
        assert!(
            fat_size > thin_size * 50,
            "the fat file has to actually be larger, and it is {fat_size} against {thin_size}"
        );

        assert_eq!(thin.opening.reads, fat.opening.reads, "the same reads either way");
        assert_eq!(thin.pages, 0, "opening read a page");
        assert_eq!(fat.pages, 0, "opening read a page");
        assert_eq!(thin.indexes, 0, "opening read an index");
        assert_eq!(fat.indexes, 0, "opening read an index");
        // Not exactly equal, because a directory holds offsets and a larger file has larger ones,
        // and a handful of bytes of varint is not somebody loading statistics. A factor is.
        assert!(
            fat.opening.bytes < thin.opening.bytes * 2,
            "opening the thin file read {} bytes and the fat one read {}",
            thin.opening.bytes,
            fat.opening.bytes
        );
    }

    /// The reads a file costs to open are fixed by its shape and not by what ran before.
    ///
    /// `spec/stats/04-in-memory.md` section 4.3, which is the rule that keeps a plan reproducible:
    /// the plan is a function of the data, the generation and the settings, and never of what
    /// happened to be in cache. Opening the same file twice in the same process has to cost the
    /// same, because a second open that read less would be an open that was about to plan
    /// differently.
    #[test]
    fn two_opens_of_one_file_cost_the_same_and_the_second_is_not_cheaper() {
        let path = path("open-twice");
        let mut writer =
            Writer::create(&path, "items", vec![Field::required("id", LogicalType::Integer)])
                .expect("new file");
        for part in 0..STRIPE_PARTS * 3 {
            let chunk = Chunk::new(vec![
                Vector::from_values(LogicalType::Integer, &[Value::Integer(part as i32)])
                    .expect("integers"),
            ])
            .expect("matching rows");
            writer.append(&chunk).expect("one part");
        }
        writer.finish().expect("commit");

        let first = Reader::open(&path).expect("open");
        // A whole scan in between, so the operating system's page cache is as warm as it gets and
        // anything that consulted it would show up in the second open.
        for part in 0..first.parts() {
            first.read(part, &[0]).expect("a part");
        }
        assert!(first.reads().pages > 0, "the scan has to have read something");
        let second = Reader::open(&path).expect("open again");

        assert_eq!(first.reads().opening, second.reads().opening);
        assert_eq!(
            second.reads().pages,
            0,
            "the second open read a page off the back of the first"
        );
        assert_eq!(second.reads().indexes, 0, "the second open read an index it inherited");
        fs::remove_file(path).expect("remove scratch file");
    }

    /// A scan reads a stripe's index once for the whole scan, not once per part that misses.
    ///
    /// The page cache holds four stripes and an index used to ride inside it, so a table with more
    /// stripes than that read the index again every time a stripe came back around. The index is a
    /// few hundred bytes and the page is a quarter of a megabyte, which is why they are now under
    /// different budgets. This is the test that keeps them there, since the saving is small enough
    /// that nothing in a benchmark would notice it going away again.
    #[test]
    fn an_index_is_read_once_per_stripe_however_often_the_page_is_evicted() {
        let path = path("index-cache");
        let mut writer =
            Writer::create(&path, "items", vec![Field::required("id", LogicalType::Integer)])
                .expect("new file");
        let parts = STRIPE_PARTS * (CACHED_STRIPES_PER_COLUMN + 2);
        for part in 0..parts {
            let id = part as i32;
            let chunk = Chunk::new(vec![
                Vector::from_values(LogicalType::Integer, &[Value::Integer(id)]).expect("integers"),
            ])
            .expect("matching rows");
            writer.append(&chunk).expect("one part");
        }
        writer.finish().expect("commit");

        let reader = Reader::open(&path).expect("reopen from disk");
        let stripes = reader.table().stripes().len();
        assert!(stripes > CACHED_STRIPES_PER_COLUMN, "the page cache has to be too small for this");
        // Twice over, so that the second pass finds every page evicted and every index kept.
        for _ in 0..2 {
            for part in 0..parts {
                let chunk = reader.read(part, &[0]).expect("a part");
                assert_eq!(chunk.value_at(0, 0), Value::Integer(part as i32));
            }
        }
        assert_eq!(reader.indexes.load(Atomic::Relaxed), stripes, "one index read per stripe");
        assert!(
            reader.pages.load(Atomic::Relaxed) > stripes,
            "the pages are the ones that get read again, which is what makes the index count mean \
             something"
        );
        fs::remove_file(path).expect("remove scratch file");
    }

    /// A page stays in memory from one scan to the next while the pool has room for it, and a
    /// table that is being read takes room from one that is not, down to the floor and no further.
    ///
    /// This is what the pool is for. Each reader lives as long as its database, so a second query
    /// over the same table should find every page it read the first time, and before the pool it
    /// found four stripes a column and read the rest off the file again.
    #[test]
    fn a_pool_keeps_pages_between_scans_and_gives_them_to_the_table_being_read() {
        let path = path("page-pool");
        let parts = STRIPE_PARTS * (CACHED_STRIPES_PER_COLUMN * 2 + 2);
        let fields = || vec![Field::required("id", LogicalType::Integer)];
        let mut writer = Writer::create(&path, "a", fields()).expect("new file");
        for table in ["a", "b"] {
            if table == "b" {
                writer = writer.next("b".to_string(), fields()).expect("a second table");
            }
            for part in 0..parts {
                let chunk = Chunk::new(vec![
                    Vector::from_values(LogicalType::Integer, &[Value::Integer(part as i32)])
                        .expect("integers"),
                ])
                .expect("matching rows");
                writer.append(&chunk).expect("one part");
            }
        }
        writer.finish().expect("commit");

        let pool = PagePool::new(usize::MAX);
        let catalog = Catalog::open_in(&path, &pool).expect("the file opens");
        let (a, b) = (catalog.table("a").expect("a"), catalog.table("b").expect("b"));
        let stripes = a.table().stripes().len();
        assert!(
            stripes > CACHED_STRIPES_PER_COLUMN * 2,
            "the floor has to be smaller than a table"
        );
        let scan = |reader: &Reader| {
            for part in 0..parts {
                let chunk = reader.read(part, &[0]).expect("a part");
                assert_eq!(chunk.value_at(0, 0), Value::Integer(part as i32));
            }
        };
        // The first scan keeps the newest pages of the floor and no more, so the second reads the
        // rest again and keeps them, and the third reads nothing.
        scan(&a);
        assert_eq!(pool.bytes(), 0, "a page read once is not the pool's");
        scan(&a);
        let twice = stripes * 2 - CACHED_STRIPES_PER_COLUMN;
        assert_eq!(a.pages.load(Atomic::Relaxed), twice, "the second scan reads the rest again");
        scan(&a);
        assert_eq!(a.pages.load(Atomic::Relaxed), twice, "the third scan reads nothing");
        let one = pool.bytes();
        assert!(one > 0, "the pool counts what the reader holds");

        // Room for one table. Reading the other takes the first one's pages down to its floor.
        pool.budget.store(one, Atomic::Relaxed);
        scan(&b);
        scan(&b);
        assert_eq!(b.pages.load(Atomic::Relaxed), twice, "a page is never let go while in use");
        assert_eq!(a.cache.held[0].load(Atomic::Relaxed), CACHED_STRIPES_PER_COLUMN);
        let column = a.cache.columns[0].lock().expect("the column");
        let held = column.pages.iter().flatten().count();
        assert_eq!(
            held,
            CACHED_STRIPES_PER_COLUMN + column.passing.len(),
            "the count and the slots agree"
        );
        drop(column);

        // A reader that goes takes its pages out of the count with it.
        drop((a, b, catalog));
        let c = Catalog::open_in(&path, &pool).expect("again").table("a").expect("a");
        scan(&c);
        scan(&c);
        assert!(pool.bytes() <= one, "only what the live reader holds is counted");
        fs::remove_file(path).expect("remove scratch file");
    }

    /// A worker per stripe reads its stripe once, once the cache has been told how many there are.
    ///
    /// This is the shape a scan has when it hands out a whole stripe per morsel rather than a part.
    /// Nobody races for a page any more, but every worker holds a different one for the length of a
    /// stripe, so a cache that keeps four pages while eight workers are in eight stripes evicts
    /// every one of them before its owner has finished with it, and the owner reads a quarter of a
    /// megabyte again for the next part. The barrier is what makes that certain rather than likely:
    /// without it a worker can run a whole stripe before the next one starts and never collide.
    #[test]
    fn a_worker_per_stripe_reads_its_page_once_when_the_cache_was_told_to_expect_it() {
        let workers = CACHED_STRIPES_PER_COLUMN + 4;
        let path = path("stripe-per-worker");
        let mut writer =
            Writer::create(&path, "items", vec![Field::required("id", LogicalType::Integer)])
                .expect("new file");
        for part in 0..STRIPE_PARTS * workers {
            let chunk = Chunk::new(vec![
                Vector::from_values(LogicalType::Integer, &[Value::Integer(part as i32)])
                    .expect("integers"),
            ])
            .expect("matching rows");
            writer.append(&chunk).expect("one part");
        }
        writer.finish().expect("commit");

        let read = |told: bool| {
            let reader = Reader::open(&path).expect("reopen from disk");
            assert_eq!(reader.table().stripes().len(), workers, "a stripe per worker");
            if told {
                reader.keep_stripes(workers);
            }
            let barrier = std::sync::Barrier::new(workers);
            std::thread::scope(|scope| {
                for (worker, run) in reader.stripe_parts().into_iter().enumerate() {
                    let reader = &reader;
                    let barrier = &barrier;
                    scope.spawn(move || {
                        for part in run {
                            barrier.wait();
                            let chunk = reader.read(part, &[0]).expect("a part of my own stripe");
                            assert_eq!(chunk.value_at(0, 0), Value::Integer(part as i32));
                        }
                        assert!(worker < workers);
                    });
                }
            });
            reader.pages.load(Atomic::Relaxed)
        };

        assert_eq!(read(true), workers, "one page read per stripe and no more");
        assert!(read(false) > workers, "a cache that small is read again on every part");
        fs::remove_file(path).expect("remove scratch file");
    }

    /// A damaged index page is caught before anything decodes a part out of it.
    ///
    /// The index is the one structure a reader trusts to find bytes with, so it carries a checksum
    /// per column section rather than one for the page, and this is what says that check runs.
    #[test]
    fn a_damaged_index_page_is_an_error() {
        let path = path("damaged-index");
        let mut writer =
            Writer::create(&path, "items", vec![Field::required("id", LogicalType::Integer)])
                .expect("new file");
        writer.append(&sample_ids()).expect("first part");
        writer.append(&sample_ids()).expect("second part");
        writer.finish().expect("commit");

        let reader = Reader::open(&path).expect("valid directory");
        let index = reader.table.stripes[0].index;
        let mut byte = [0; 1];
        read_at(&reader.file, index.offset, &mut byte).expect("the first part length");
        let mut file = OpenOptions::new().write(true).open(&path).expect("open index page");
        file.seek(SeekFrom::Start(index.offset)).expect("index start");
        file.write_all(&[!byte[0]]).expect("damage the first part length");
        let error = reader.read(1, &[0]).expect_err("a damaged index must not be used");
        assert!(error.message().contains("index page section checksum differs"), "{error}");
        fs::remove_file(path).expect("remove scratch file");
    }

    /// Every integer width the format knows about, written and read back.
    ///
    /// The unsigned ones are the reason ClickBench can be stored at all: `hits` types `EventDate`
    /// as `USMALLINT`, and one unsupported column meant the whole table was refused. The extremes
    /// are in here on purpose, because a width that round trips through the wrong signedness only
    /// goes wrong at the end of its range.
    #[test]
    fn every_integer_width_round_trips_through_a_page() {
        let path = path("integer-widths");
        let columns = [
            (LogicalType::TinyInt, vec![Value::TinyInt(i8::MIN), Value::TinyInt(i8::MAX)]),
            (LogicalType::UTinyInt, vec![Value::UTinyInt(0), Value::UTinyInt(u8::MAX)]),
            (LogicalType::SmallInt, vec![Value::SmallInt(i16::MIN), Value::SmallInt(i16::MAX)]),
            (LogicalType::USmallInt, vec![Value::USmallInt(0), Value::USmallInt(u16::MAX)]),
            (LogicalType::Integer, vec![Value::Integer(i32::MIN), Value::Integer(i32::MAX)]),
            (LogicalType::UInteger, vec![Value::UInteger(0), Value::UInteger(u32::MAX)]),
            (LogicalType::BigInt, vec![Value::BigInt(i64::MIN), Value::BigInt(i64::MAX)]),
            (LogicalType::UBigInt, vec![Value::UBigInt(0), Value::UBigInt(u64::MAX)]),
        ];
        let fields = columns
            .iter()
            .enumerate()
            .map(|(at, (ty, _))| Field::required(format!("c{at}"), ty.clone()))
            .collect::<Vec<_>>();
        let vectors = columns
            .iter()
            .map(|(ty, values)| Vector::from_values(ty.clone(), values).expect("a vector"))
            .collect::<Vec<_>>();
        let mut writer = Writer::create(&path, "widths", fields).expect("new file");
        writer.append(&Chunk::new(vectors).expect("matching rows")).expect("one stripe");
        writer.finish().expect("commit");

        let reader = Reader::open(&path).expect("reopen from disk");
        let wanted = (0..columns.len()).collect::<Vec<_>>();
        let read = reader.read(0, &wanted).expect("every column");
        assert_eq!(read.len(), 2);
        // row at a time: each column has its own type and its own pair of extremes.
        for (at, (ty, values)) in columns.iter().enumerate() {
            assert_eq!(read.value_at(0, at), values[0], "the low end of {ty}");
            assert_eq!(read.value_at(1, at), values[1], "the high end of {ty}");
        }
        fs::remove_file(path).expect("remove scratch file");
    }

    /// The rest of the fixed width types, and the byte strings, written and read back.
    ///
    /// The extremes again, and for a float that means more than the ends of the range. Negative
    /// zero and a NaN are the two values that go through an encoder unnoticed and come back
    /// different, so they are here on purpose, and the NaN is compared by its bits rather than by
    /// `==`, which a NaN fails against itself.
    ///
    /// A blob is here beside them because it is the same round trip asked of bytes that are not
    /// text. The value in it is not UTF-8, so a path that reads a payload as a string on the way
    /// past turns this test red rather than turning a user's column into nulls.
    #[test]
    fn every_other_type_the_format_knows_round_trips_through_a_page() {
        let path = path("other-types");
        let columns = [
            (LogicalType::Float, vec![Value::Float(f32::MIN), Value::Float(-0.0)]),
            (LogicalType::Double, vec![Value::Double(f64::MIN), Value::Double(f64::MAX)]),
            (LogicalType::HugeInt, vec![Value::HugeInt(i128::MIN), Value::HugeInt(i128::MAX)]),
            (LogicalType::UHugeInt, vec![Value::UHugeInt(0), Value::UHugeInt(u128::MAX)]),
            (LogicalType::Time, vec![Value::Time(0), Value::Time(86_399_999_999)]),
            (LogicalType::TimeTz, vec![Value::TimeTz(-50_400_000_000), Value::TimeTz(0)]),
            (
                LogicalType::TimestampTz,
                vec![Value::TimestampTz(i64::MIN + 1), Value::TimestampTz(i64::MAX)],
            ),
            (
                LogicalType::Interval,
                vec![
                    Value::Interval { months: i32::MIN, days: i32::MAX, micros: i64::MIN },
                    Value::Interval { months: 13, days: -1, micros: 1 },
                ],
            ),
            (
                LogicalType::Blob,
                vec![Value::Blob(vec![0, 0xff, 0x80, 0xfe]), Value::Blob(Vec::new())],
            ),
        ];
        let fields = columns
            .iter()
            .enumerate()
            .map(|(at, (ty, _))| Field::required(format!("c{at}"), ty.clone()))
            .collect::<Vec<_>>();
        let vectors = columns
            .iter()
            .map(|(ty, values)| Vector::from_values(ty.clone(), values).expect("a vector"))
            .collect::<Vec<_>>();
        let mut writer = Writer::create(&path, "others", fields).expect("new file");
        writer.append(&Chunk::new(vectors).expect("matching rows")).expect("one stripe");
        writer.finish().expect("commit");

        let reader = Reader::open(&path).expect("reopen from disk");
        let wanted = (0..columns.len()).collect::<Vec<_>>();
        let read = reader.read(0, &wanted).expect("every column");
        assert_eq!(read.len(), 2);
        for (at, (ty, values)) in columns.iter().enumerate() {
            assert_eq!(read.value_at(0, at), values[0], "the low end of {ty}");
            assert_eq!(read.value_at(1, at), values[1], "the high end of {ty}");
        }
        // A float keeps its sign through a zero, which `==` says nothing about because negative
        // zero and zero compare equal.
        let Value::Float(zero) = read.value_at(1, 0) else { panic!("a float stays a float") };
        assert!(zero.is_sign_negative(), "a negative zero came back as {zero}");

        fs::remove_file(path).expect("remove scratch file");
    }

    /// A NaN is still a NaN after a trip through a page.
    ///
    /// Apart from the other floats because it cannot be asserted the same way. A NaN is not equal
    /// to itself, so a comparison against the value that was written passes for every NaN and for
    /// nothing else, which is the one assertion that would not catch a page that lost it.
    #[test]
    fn a_nan_survives_being_written_down() {
        let path = path("nan");
        let nan = Vector::from_values(LogicalType::Double, &[Value::Double(f64::NAN)])
            .expect("a NaN vector");
        let mut writer =
            Writer::create(&path, "nan", vec![Field::required("d", LogicalType::Double)])
                .expect("new file");
        writer.append(&Chunk::new(vec![nan]).expect("one column")).expect("one stripe");
        writer.finish().expect("commit");
        let read = Reader::open(&path).expect("reopen").read(0, &[0]).expect("the column");
        let Value::Double(back) = read.value_at(0, 0) else { panic!("a double stays a double") };
        assert!(back.is_nan(), "a NaN came back as {back}");
        fs::remove_file(path).expect("remove scratch file");
    }

    /// A uuid and a bit string, which have no `Value` arm of their own and are checked as bits.
    ///
    /// A uuid is the 128 bit lane and a bit string is bytes, and neither of them reads back as
    /// anything in `Value` today, so asking for a value here would compare two nulls and pass
    /// whatever the file held. The data underneath is what the storage promise is about, so that is
    /// what this reads.
    #[test]
    fn a_uuid_and_a_bit_string_come_back_as_the_bits_that_went_in() {
        let path = path("uuid-and-bit");
        let uuids = vec![0_i128, i128::MIN, -1];
        let mut bits = StringColumn::new();
        for value in [&b"\x02\xff"[..], &b""[..], &b"\x00\x01\x02\x03\x04\x05"[..]] {
            bits.push_bytes(value);
        }
        let expected = bits.clone();
        let fields =
            vec![Field::required("u", LogicalType::Uuid), Field::required("b", LogicalType::Bit)];
        let vectors = vec![
            Vector::flat(LogicalType::Uuid, Data::Int128(uuids.clone().into())).expect("uuids"),
            Vector::flat(LogicalType::Bit, Data::Varlen(bits)).expect("bit strings"),
        ];
        let mut writer = Writer::create(&path, "ids", fields).expect("new file");
        writer.append(&Chunk::new(vectors).expect("matching rows")).expect("one stripe");
        writer.finish().expect("commit");

        let reader = Reader::open(&path).expect("reopen from disk");
        let read = reader.read(0, &[0, 1]).expect("both columns").flatten().expect("flat");
        let Some(Data::Int128(back)) = read.column(0).expect("the uuids").data() else {
            panic!("a uuid column is the 128 bit lane")
        };
        assert_eq!(back.as_slice(), uuids.as_slice());
        let Some(Data::Varlen(back)) = read.column(1).expect("the bits").data() else {
            panic!("a bit column is bytes")
        };
        for row in 0..expected.len() {
            assert_eq!(back.bytes(row), expected.bytes(row), "row {row} of the bit column");
        }
        fs::remove_file(path).expect("remove scratch file");
    }

    /// Counting a run at once has to leave the candidate table exactly where counting its rows one
    /// at a time would, including once the table is full and a run is turned away row by row.
    #[test]
    fn a_run_counted_at_once_leaves_the_candidates_a_row_at_a_time_would() {
        let mut rows: Vec<Option<u64>> = Vec::new();
        let mut state = 0x2545_f491_4f6c_dd1d_u64;
        for index in 0..400_000_u64 {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            let times = 1 + (state % 7) as usize;
            let bits = match state % 11 {
                0 => None,
                1..=3 => Some(state % 16),
                _ => Some(index.wrapping_mul(0x9e37_79b9_7f4a_7c15)),
            };
            rows.extend(std::iter::repeat_n(bits, times));
        }
        let mut by_row = Candidates::default();
        for &bits in &rows {
            by_row.add(bits, 1);
        }
        let mut by_run = Candidates::default();
        let mut run = Run::default();
        let mut runs = 0_usize;
        for &bits in &rows {
            if let Some((bits, times)) = run.push(bits) {
                by_run.add(bits, times);
                runs += 1;
            }
        }
        if let Some((bits, times)) = run.take() {
            by_run.add(bits, times);
        }
        assert!(runs < rows.len() / 2, "the rows came in runs");
        assert!(by_row.decrements > 0, "the table filled and turned values away");
        assert_eq!(sorted_candidates(&by_run), sorted_candidates(&by_row));
        assert_eq!(by_run.nulls, by_row.nulls);
        assert_eq!(by_run.decrements, by_row.decrements);
    }

    fn sorted_candidates(candidates: &Candidates) -> Vec<(u64, u32)> {
        let mut pairs = candidates.pairs().collect::<Vec<_>>();
        pairs.sort_unstable();
        assert_eq!(pairs.len(), candidates.held, "the count of held slots drifted");
        pairs
    }

    /// The Misra-Gries table as it was written over a `HashMap`, kept as the oracle the open
    /// addressed one has to agree with.
    #[derive(Default)]
    struct MapCandidates {
        counts: HashMap<u64, u32>,
        nulls: u32,
        decrements: u64,
    }

    impl MapCandidates {
        fn add(&mut self, bits: Option<u64>, mut times: u32) {
            while times > 0 {
                let held = match bits {
                    Some(bits) => self.counts.get_mut(&bits),
                    None if self.nulls != 0 => Some(&mut self.nulls),
                    None => None,
                };
                if let Some(count) = held {
                    *count = count.saturating_add(times);
                    return;
                }
                if self.counts.len() + usize::from(self.nulls != 0) < FREQUENCY_CANDIDATES {
                    match bits {
                        Some(bits) => {
                            self.counts.insert(bits, times);
                        }
                        None => self.nulls = times,
                    }
                    return;
                }
                self.counts.retain(|_, count| {
                    *count -= 1;
                    *count != 0
                });
                self.nulls = self.nulls.saturating_sub(1);
                self.decrements = self.decrements.saturating_add(1);
                times -= 1;
            }
        }
    }

    /// Near unique values, a few heavy ones, nulls, and runs, through enough rows that the table
    /// fills, grows through every size and is decremented many times over. Both tables have to hold
    /// the same candidates with the same counts at the end, and at points along the way.
    #[test]
    fn the_open_addressed_candidates_agree_with_the_map_they_replaced() {
        for seed in [0x2545_f491_4f6c_dd1d_u64, 0x9e37_79b9_7f4a_7c15, 7] {
            let mut table = Candidates::default();
            let mut oracle = MapCandidates::default();
            let mut state = seed;
            for index in 0..300_000_u64 {
                state ^= state << 13;
                state ^= state >> 7;
                state ^= state << 17;
                let bits = match state % 13 {
                    0 => None,
                    1..=4 => Some(state % 40),
                    5 => Some((index % 1000) * 1_000_000),
                    _ => Some(state),
                };
                let times = 1 + (state >> 60) as u32 % 3;
                table.add(bits, times);
                oracle.add(bits, times);
                if index % 50_000 == 0 {
                    let mut expected =
                        oracle.counts.iter().map(|(&b, &c)| (b, c)).collect::<Vec<_>>();
                    expected.sort_unstable();
                    assert_eq!(sorted_candidates(&table), expected, "seed {seed} row {index}");
                }
            }
            let mut expected = oracle.counts.iter().map(|(&b, &c)| (b, c)).collect::<Vec<_>>();
            expected.sort_unstable();
            assert_eq!(sorted_candidates(&table), expected, "seed {seed}");
            assert_eq!(table.nulls, oracle.nulls, "seed {seed}");
            assert_eq!(table.decrements, oracle.decrements, "seed {seed}");
            assert!(table.decrements > 0, "seed {seed} never filled the table");
            for &(bits, _) in &expected {
                assert!(table.position(bits).is_some(), "seed {seed} lost {bits}");
            }
        }
    }

    #[test]
    fn numeric_frequency_candidates_keep_bounded_row_ordinals() {
        let path = path("frequency-ordinals");
        let mut writer =
            Writer::create(&path, "items", vec![Field::required("id", LogicalType::BigInt)])
                .expect("new file");
        let mut values = Vec::new();
        for leader in 0..10_i64 {
            values.extend(std::iter::repeat_n(leader, 100));
        }
        values.extend(1_000_i64..41_000);
        for part in values.chunks(1_024) {
            let vector = Vector::flat(LogicalType::BigInt, Data::Int64(part.to_vec().into()))
                .expect("big integers");
            writer.append(&Chunk::new(vec![vector]).expect("one column")).expect("one stripe");
        }
        writer.finish().expect("commit");

        let reader = Reader::open(&path).expect("reopen from disk");
        let occurrences =
            reader.frequency_occurrences(0).expect("valid metadata").expect("bounded ordinals");
        assert!(occurrences.omitted_max < 100);
        assert!(occurrences.ordinals.len() <= FREQUENCY_ORDINALS);
        assert_eq!(occurrences.anchor_indices.len(), occurrences.ordinals.len());
        assert!(occurrences.ordinals.windows(2).all(|pair| pair[0] < pair[1]));
        assert_eq!(&occurrences.ordinals[..1_000], &(0_u64..1_000).collect::<Vec<_>>());
        assert_eq!(
            &occurrences.anchor_indices[..1_000]
                .iter()
                .map(|&entry| occurrences.anchors[entry as usize].clone())
                .collect::<Vec<_>>(),
            &(0_i64..10)
                .flat_map(|leader| std::iter::repeat_n(Value::BigInt(leader), 100))
                .collect::<Vec<_>>()
        );
        fs::remove_file(path).expect("remove scratch file");
    }

    #[test]
    fn numeric_frequencies_count_nulls_and_values_past_the_top_of_bigint() {
        // Ten leaders, then more unique values than the candidate table holds, so the first pass
        // has to decrement and the counts come from the recount. The unsigned leaders sit above
        // `i64::MAX`, where reading the bits as signed would give a different value, and the signed
        // ones are negative, where reading them as unsigned would.
        let path = path("frequency-bits");
        let mut writer = Writer::create(
            &path,
            "items",
            vec![Field::new("u", LogicalType::UBigInt), Field::new("s", LogicalType::BigInt)],
        )
        .expect("new file");
        let mut rows = Vec::new();
        let mut leaders = Vec::new();
        for leader in 0..10_u64 {
            let count = 300 - leader * 10;
            let (unsigned, signed) = if leader == 0 {
                (Value::Null, Value::Null)
            } else {
                (Value::UBigInt(u64::MAX - leader), Value::BigInt(-(leader as i64)))
            };
            rows.extend(std::iter::repeat_n((unsigned.clone(), signed.clone()), count as usize));
            leaders.push(((unsigned, count), (signed, count)));
        }
        rows.extend((1_000..41_000_u64).map(|id| (Value::UBigInt(id), Value::BigInt(id as i64))));
        for part in rows.chunks(1_024) {
            let unsigned = part.iter().map(|(value, _)| value.clone()).collect::<Vec<_>>();
            let signed = part.iter().map(|(_, value)| value.clone()).collect::<Vec<_>>();
            let chunk = Chunk::new(vec![
                Vector::from_values(LogicalType::UBigInt, &unsigned).expect("unsigned"),
                Vector::from_values(LogicalType::BigInt, &signed).expect("signed"),
            ])
            .expect("matching columns");
            writer.append(&chunk).expect("rows");
        }
        writer.finish().expect("commit");

        let reader = Reader::open(&path).expect("reopen from disk");
        for column in 0..2 {
            let prefix =
                reader.frequency_prefix(column).expect("valid metadata").expect("a synopsis");
            let wanted = leaders
                .iter()
                .map(|(unsigned, signed)| if column == 0 { unsigned } else { signed })
                .cloned()
                .collect::<Vec<_>>();
            assert_eq!(&prefix.entries[..10], &wanted[..], "column {column}");
            assert!(prefix.omitted_max < 210, "column {column}");
            assert_eq!(
                reader.distinct_values(column).expect("valid metadata"),
                Some(9 + 40_000),
                "column {column}"
            );
        }
        fs::remove_file(path).expect("remove scratch file");
    }

    #[test]
    fn a_narrow_column_takes_its_frequencies_from_the_tally_and_they_match_the_rows() {
        // Every column here has fewer distinct values than the tally holds, so the close takes its
        // counts from the gather rather than reading the pages back. The types are the ones whose
        // bits could come out wrong on that road: a negative tiny integer that has to be sign
        // extended, an unsigned one past the top of `INTEGER`, a date and a timestamp. A null every
        // thirteenth row checks that the nulls come from the pass and not from the list.
        let path = path("frequency-tally");
        let types = [
            LogicalType::TinyInt,
            LogicalType::UInteger,
            LogicalType::Date,
            LogicalType::Timestamp,
        ];
        let value = |ty: &LogicalType, at: i64| match ty {
            LogicalType::TinyInt => Value::TinyInt((at % 250 - 125) as i8),
            LogicalType::UInteger => Value::UInteger(u32::MAX - at as u32),
            LogicalType::Date => Value::Date(19_000 - at as i32),
            _ => Value::Timestamp(1_700_000_000_000_000 - at * 1_000_003),
        };
        let fields = types
            .iter()
            .enumerate()
            .map(|(at, ty)| Field::new(format!("c{at}"), ty.clone()))
            .collect::<Vec<_>>();
        let mut writer = Writer::create(&path, "items", fields).expect("new file");
        let mut rows = Vec::new();
        for at in 0..250_i64 {
            for _ in 0..=(at % 37) {
                rows.push(if rows.len() % 13 == 0 { None } else { Some(at) });
            }
        }
        for part in rows.chunks(1_000) {
            let columns = types
                .iter()
                .map(|ty| {
                    let values = part
                        .iter()
                        .map(|row| row.map_or(Value::Null, |at| value(ty, at)))
                        .collect::<Vec<_>>();
                    Vector::from_values(ty.clone(), &values).expect("a column")
                })
                .collect();
            writer.append(&Chunk::new(columns).expect("matching columns")).expect("rows");
        }
        writer.finish().expect("commit");

        let reader = Reader::open(&path).expect("reopen from disk");
        for (column, ty) in types.iter().enumerate() {
            let mut counts = HashMap::<Option<i64>, u64>::new();
            for row in &rows {
                *counts.entry(*row).or_default() += 1;
            }
            let wanted = counts
                .into_iter()
                .map(|(row, count)| (row.map_or(Value::Null, |at| value(ty, at)), count))
                .collect::<Vec<_>>();
            let prefix =
                reader.frequency_prefix(column).expect("valid metadata").expect("a synopsis");
            assert_eq!(prefix.entries.len(), 2, "column {column}");
            assert!(prefix.omitted_max > 0, "column {column}");
            for (value, count) in &prefix.entries {
                let held =
                    wanted.iter().find(|(wanted, _)| wanted == value).map(|(_, count)| count);
                assert_eq!(held, Some(count), "column {column} value {value:?}");
            }
            assert!(prefix.entries.windows(2).all(|pair| pair[0].1 >= pair[1].1));
            assert_eq!(
                reader.distinct_values(column).expect("valid metadata"),
                Some(wanted.len() as u64 - 1),
                "column {column}"
            );
        }
        fs::remove_file(path).expect("remove scratch file");
    }

    #[test]
    fn distinct_counts_are_exact_either_side_of_a_full_candidate_table() {
        // The count comes from the candidate table while it has room and from the set once it
        // fills, so the sizes around the fill, with and without a null taking a place, are where a
        // value could be counted twice or missed. Zero is in every column because the set keeps it
        // apart from the other values, and every value comes back later to be counted again.
        let edge = FREQUENCY_CANDIDATES as i64;
        for distinct in [0, 1, 7, edge - 2, edge - 1, edge, edge + 1, edge + 2, 3 * edge] {
            for with_null in [false, true] {
                let path = path("distinct-edge");
                let mut writer =
                    Writer::create(&path, "items", vec![Field::new("id", LogicalType::BigInt)])
                        .expect("new file");
                let mut values = Vec::new();
                for round in 0..2 {
                    for value in 0..distinct {
                        let repeat = if round == 0 { 1 + (value % 3) as usize } else { 1 };
                        values.extend(std::iter::repeat_n(
                            Value::BigInt(value * 7_919 % distinct),
                            repeat,
                        ));
                        if with_null && value % 1_000 == 0 {
                            values.push(Value::Null);
                        }
                    }
                }
                if with_null {
                    values.push(Value::Null);
                }
                for part in values.chunks(1_024) {
                    let chunk = Chunk::new(vec![
                        Vector::from_values(LogicalType::BigInt, part).expect("ids"),
                    ])
                    .expect("one column");
                    writer.append(&chunk).expect("rows");
                }
                writer.finish().expect("commit");
                let reader = Reader::open(&path).expect("reopen from disk");
                assert_eq!(
                    reader.distinct_values(0).expect("valid metadata"),
                    Some(distinct as u64),
                    "{distinct} values, null {with_null}"
                );
                fs::remove_file(path).expect("remove scratch file");
            }
        }
    }

    #[test]
    fn narrow_nonzero_count_matches_the_full_reader_across_stripes() {
        let path = path("quick-nonzero");
        let mut writer = Writer::create(
            &path,
            "items",
            vec![Field::new("label", LogicalType::Varchar), Field::new("id", LogicalType::Integer)],
        )
        .expect("create");
        for ids in [
            &[Value::Integer(0), Value::Null, Value::Integer(3)][..],
            &[Value::Integer(0), Value::Integer(7), Value::Null][..],
        ] {
            let labels = vec![Value::Varchar("same".into()); ids.len()];
            writer
                .append(
                    &Chunk::new(vec![
                        Vector::from_values(LogicalType::Varchar, &labels).expect("labels"),
                        Vector::from_values(LogicalType::Integer, ids).expect("ids"),
                    ])
                    .expect("chunk"),
                )
                .expect("append");
        }
        writer.finish().expect("finish");
        let catalog = Catalog::open(&path).expect("catalog");
        assert_eq!(catalog.entries[0].nonzero, vec![None, None]);
        assert_eq!(catalog.entries[0].aggregates, vec![None, Some((10, 4))]);
        assert_eq!(catalog.entries[0].distincts, vec![Some(1), Some(3)]);
        assert_eq!(catalog.exact_numeric_frequencies("items", 1).expect("frequencies"), None);
        let prefix = catalog
            .table("items")
            .expect("reader")
            .frequency_prefix(1)
            .expect("valid metadata")
            .expect("partial frequencies");
        assert_eq!(prefix.entries, vec![(Value::Null, 2), (Value::Integer(0), 2)]);
        assert_eq!(prefix.omitted_max, 1);
        assert_eq!(catalog.distinct_count("items", 1).expect("distinct count"), Some(3));
        assert_eq!(
            catalog.integer_extremes("items", 1).expect("extremes"),
            Some(IntegerExtremes::Values { low: 0, high: 7 })
        );
        assert_eq!(
            catalog.aggregate_sums("items", &[1]).expect("catalog sums"),
            Some(CertifiedSums { columns: vec![(10, 4)], rows: 6 })
        );
        assert_eq!(catalog.nonzero_count("items", 1).expect("quick count"), Some(2));
        let mut legacy = catalog.clone();
        Arc::make_mut(&mut legacy.entries)[0].nonzero[1] = Some(999);
        assert_eq!(legacy.nonzero_count("items", 1).expect("ignore legacy count"), Some(2));
        Arc::make_mut(&mut legacy.entries)[0].frequencies[1] = None;
        assert_eq!(legacy.nonzero_count("items", 1).expect("directory fallback"), Some(2));
        Writer::certify_counts(&path).expect("recertify");
        assert_eq!(
            Catalog::open(&path).expect("reopen").nonzero_count("items", 1).expect("count"),
            Some(2)
        );
        assert_eq!(
            Catalog::open(&path).expect("reopen").aggregate_sums("items", &[1]).expect("sums"),
            Some(CertifiedSums { columns: vec![(10, 4)], rows: 6 })
        );
        assert_eq!(
            Catalog::open(&path).expect("reopen").distinct_count("items", 1).expect("distinct"),
            Some(3)
        );
        assert_eq!(
            Catalog::open(&path).expect("reopen").integer_extremes("items", 1).expect("ends"),
            Some(IntegerExtremes::Values { low: 0, high: 7 })
        );
        assert_eq!(
            Catalog::open(&path)
                .expect("reopen")
                .exact_numeric_frequencies("items", 1)
                .expect("frequencies"),
            None
        );
        assert_eq!(catalog.table("items").expect("reader").null_count(1).expect("nulls"), 2);
        fs::remove_file(path).expect("remove scratch file");
    }

    #[test]
    fn numeric_string_pair_leaders_are_certified_in_the_directory() {
        let path = path("pair-frequencies");
        let mut pairs = Vec::new();
        pairs.extend(std::iter::repeat_n((1_i64, "alpha".to_string()), 100));
        pairs.extend(std::iter::repeat_n((1_i64, "beta".to_string()), 50));
        pairs.extend(std::iter::repeat_n((2_i64, "gamma".to_string()), 40));
        pairs.extend((1_000_i64..1_600).map(|id| (id, format!("tail {id}"))));
        let mut writer = Writer::create(
            &path,
            "items",
            vec![
                Field::required("id", LogicalType::BigInt),
                Field::required("phrase", LogicalType::Varchar),
            ],
        )
        .expect("new file");
        for part in pairs.chunks(1_024) {
            let ids = part.iter().map(|(id, _)| Value::BigInt(*id)).collect::<Vec<_>>();
            let phrases =
                part.iter().map(|(_, phrase)| Value::Varchar(phrase.clone())).collect::<Vec<_>>();
            writer
                .append(
                    &Chunk::new(vec![
                        Vector::from_values(LogicalType::BigInt, &ids).expect("ids"),
                        Vector::from_values(LogicalType::Varchar, &phrases).expect("phrases"),
                    ])
                    .expect("matching columns"),
                )
                .expect("rows");
        }
        writer.finish().expect("commit");

        let reader = Reader::open(&path).expect("reopen from disk");
        assert!(
            reader.table.pair_frequencies.is_empty(),
            "no query-specific pair result is stored"
        );
        fs::remove_file(path).expect("remove scratch file");
    }

    /// The bug this is here for cost a 43 GB ClickBench table and an hour of reloading it. The
    /// format went from 11 to 12, every binary built after that said "magic or major version is
    /// unsupported" about the file, and there was no way to tell from the message whether the path
    /// was wrong, the file was truncated, or it was ours and simply older. The number this build
    /// wants is the whole answer and it was the one thing the message did not carry.
    #[test]
    fn a_file_from_another_format_says_which_format_it_is() {
        let older = path("older-format");
        let mut writer =
            Writer::create(&older, "items", vec![Field::new("id", LogicalType::Integer)])
                .expect("new file");
        let chunk = Chunk::new(vec![
            Vector::flat(LogicalType::Integer, Data::Int32(vec![1, 2, 3].into()))
                .expect("integers"),
        ])
        .expect("chunk");
        writer.append(&chunk).expect("page written");
        writer.finish().expect("commit");

        // A format below the whole readable set, rather than `FORMAT - 1`, because the set has
        // more than one member now: format 22 is deliberately still readable, so the version that
        // has to be refused is the one under the oldest one accepted.
        let unreadable =
            READABLE.iter().copied().min().expect("at least one format is readable") - 1;
        let mut file = OpenOptions::new().write(true).open(&older).expect("open for the header");
        file.seek(SeekFrom::Start(8)).expect("the version follows the magic");
        file.write_all(&unreadable.to_le_bytes()).expect("write an older version");
        drop(file);
        let complaint = Reader::open(&older).expect_err("an older format is refused").to_string();
        assert!(complaint.contains(&format!("format {unreadable}")), "{complaint}");
        assert!(complaint.contains(&format!("format {FORMAT}")), "{complaint}");

        let mut file = OpenOptions::new().write(true).open(&older).expect("open for the header");
        file.seek(SeekFrom::Start(0)).expect("the magic is first");
        file.write_all(b"NOTRUDB!").expect("write another engine's magic");
        drop(file);
        let complaint = Reader::open(&older).expect_err("a foreign file is refused").to_string();
        assert!(complaint.contains("magic"), "{complaint}");
        assert!(!complaint.contains("format"), "a version has nothing to do with it: {complaint}");
        fs::remove_file(older).expect("remove scratch file");
    }

    #[test]
    fn an_unfinished_or_damaged_file_does_not_answer_with_partial_rows() {
        let unfinished = path("unfinished");
        let mut writer =
            Writer::create(&unfinished, "items", vec![Field::new("id", LogicalType::Integer)])
                .expect("new file");
        let chunk = Chunk::new(vec![
            Vector::flat(LogicalType::Integer, Data::Int32(vec![1, 2, 3].into()))
                .expect("integers"),
        ])
        .expect("chunk");
        writer.append(&chunk).expect("page written");
        drop(writer);
        assert!(Reader::open(&unfinished).is_err(), "no directory was committed");
        fs::remove_file(unfinished).expect("remove scratch file");

        let damaged = path("damaged");
        let mut writer =
            Writer::create(&damaged, "items", vec![Field::new("id", LogicalType::Integer)])
                .expect("new file");
        writer.append(&chunk).expect("page written");
        writer.finish().expect("commit");
        let reader = Reader::open(&damaged).expect("valid directory");
        let mut file =
            OpenOptions::new().write(true).open(&damaged).expect("open for a damaged page");
        file.seek(SeekFrom::Start(HEADER + 1)).expect("inside first page");
        file.write_all(&[255]).expect("damage one byte");
        assert!(reader.read(0, &[0]).is_err(), "page checksum rejects corruption");
        fs::remove_file(damaged).expect("remove scratch file");
    }

    #[test]
    fn damaged_lazy_dictionary_payload_is_an_error() {
        let path = path("damaged-dictionary");
        let mut writer = Writer::create(
            &path,
            "items",
            vec![
                Field::required("id", LogicalType::Integer),
                Field::new("text", LogicalType::Varchar),
            ],
        )
        .expect("new file");
        writer.append(&sample()).expect("stripe written");
        writer.finish().expect("commit");

        let reader = Reader::open(&path).expect("valid directory");
        let dictionary = reader.table.dictionaries[1].expect("string dictionary page");
        // Read the count out of the page rather than writing it here, so that adding something
        // else to the index does not silently turn this into a test that damages the index.
        let mut header = [0; DICTIONARY_HEADER];
        read_at(&reader.file, dictionary.offset, &mut header).expect("dictionary header");
        // The first block's start is the first word after the offsets, since the blocks are written
        // during the load and are wherever the writer was when each was encoded.
        let count = u32::from_le_bytes(header[0..4].try_into().expect("four bytes")) as usize;
        let width = u32::from_le_bytes(header[12..16].try_into().expect("four bytes"));
        assert_ne!(width & DICTIONARY_SCATTERED, 0, "the blocks say where they are");
        let bits = (width & !DICTIONARY_FLAGS) as usize;
        let mut start = [0; 8];
        let at = dictionary.offset + (DICTIONARY_HEADER + offset_bytes(count, bits)) as u64;
        read_at(&reader.file, at, &mut start).expect("the first block's start");
        let mut file = OpenOptions::new().write(true).open(&path).expect("open dictionary page");
        file.seek(SeekFrom::Start(u64::from_le_bytes(start))).expect("inside dictionary payload");
        file.write_all(&[255]).expect("damage dictionary payload");

        let chunk = reader.read(0, &[1]).expect("code page and dictionary index remain valid");
        let error =
            chunk.validate_external().expect_err("payload corruption must reach the caller");
        assert!(error.message().contains("payload checksum differs"), "{error}");
        fs::remove_file(path).expect("remove scratch file");
    }

    /// A column whose values are all different is written without a dictionary, and one whose
    /// values repeat keeps it.
    ///
    /// The two columns go in the same table and hold the same number of rows, so the only thing
    /// separating them is how much of the first stripe was a value it had not seen before. Both have
    /// to read back the values that were written, because the decision is about cost and nothing
    /// else. The file size is the other half of it: a column written without a dictionary goes
    /// through the string cascade instead, so dropping the dictionary must not turn into storing the
    /// column raw.
    #[test]
    fn a_column_of_all_different_values_is_written_without_a_dictionary() {
        let path = path("dictionary-decide");
        let rows = 20_000;
        // Long enough that storing it raw would show, and different in every row.
        let unique =
            |row: usize| format!("{row:09} a value that appears exactly once in the table");
        // The same values in the same shape, each one used forty times over.
        let repeated = |row: usize| unique(row / 40);
        let mut writer = Writer::create(
            &path,
            "items",
            vec![
                Field::required("unique", LogicalType::Varchar),
                Field::required("repeated", LogicalType::Varchar),
            ],
        )
        .expect("new file");
        for part in (0..rows).step_by(1_000) {
            let span = part..(part + 1_000).min(rows);
            let left = span.clone().map(|row| Value::Varchar(unique(row))).collect::<Vec<_>>();
            let right = span.map(|row| Value::Varchar(repeated(row))).collect::<Vec<_>>();
            writer
                .append(
                    &Chunk::new(vec![
                        Vector::from_values(LogicalType::Varchar, &left).expect("strings"),
                        Vector::from_values(LogicalType::Varchar, &right).expect("strings"),
                    ])
                    .expect("two columns"),
                )
                .expect("a part");
        }
        writer.finish().expect("commit");

        let reader = Reader::open(&path).expect("reopen from disk");
        assert!(
            reader.table.dictionaries[0].is_none(),
            "a column with no repeats has nothing to say twice"
        );
        assert!(
            reader.table.dictionaries[1].is_some(),
            "a column whose values come round again keeps its dictionary"
        );
        let mut first = 0;
        for part in 0..reader.parts() {
            let chunk = reader.read(part, &[0, 1]).expect("a part");
            for row in 0..chunk.len() {
                assert_eq!(chunk.value_at(row, 0), Value::Varchar(unique(first + row)));
                assert_eq!(chunk.value_at(row, 1), Value::Varchar(repeated(first + row)));
            }
            first += chunk.len();
        }
        assert_eq!(first, rows, "every row was read back");
        let raw = (0..rows).map(|row| unique(row).len()).sum::<usize>();
        let size = fs::metadata(&path).expect("the file is there").len() as usize;
        assert!(size < raw, "a column without a dictionary is still encoded: {size} against {raw}");
        fs::remove_file(path).expect("remove scratch file");
    }

    /// A payload of many blocks reads and checks every block of it.
    ///
    /// The test above has a dictionary of three values, which is one block, so it says nothing
    /// about a reader finding the right block among many. This one has thirty two thousand values,
    /// which is thirty two blocks, and it reads a value out of the first block and a value out of
    /// the last and then damages the last and asks for it again.
    ///
    /// Forty thousand rows over those thirty two thousand values, because a column the writer finds
    /// to be all distinct does not get a dictionary at all and there would be nothing here to test.
    /// Four rows in five holding a value the stripe has not seen before is a column that keeps one.
    /// The repeats are put at the front so that the values still arrive in order after them, which
    /// is what keeps the last part of the table on the last block of the payload.
    #[test]
    fn a_dictionary_over_many_blocks_checks_every_block_of_it() {
        let path = path("dictionary-blocks");
        let value = |row: usize| {
            let row = row.saturating_sub(8_000);
            format!("{row:07} a value long enough to be worth a payload block")
        };
        let parts = 40;
        let per_part = 1000;
        let mut writer =
            Writer::create(&path, "items", vec![Field::required("text", LogicalType::Varchar)])
                .expect("new file");
        for part in 0..parts {
            let values = (0..per_part)
                .map(|row| Value::Varchar(value(part * per_part + row)))
                .collect::<Vec<_>>();
            let chunk = Chunk::new(vec![
                Vector::from_values(LogicalType::Varchar, &values).expect("strings"),
            ])
            .expect("matching rows");
            writer.append(&chunk).expect("a part");
        }
        writer.finish().expect("commit");

        let reader = Reader::open(&path).expect("reopen from disk");
        let dictionary = reader.table.dictionaries[0].expect("string dictionary page");
        assert!(
            parts * per_part > TEXT_PAYLOAD_VALUES * 4,
            "the dictionary has to be several blocks for this to be testing anything"
        );
        for part in [0, parts - 1] {
            let chunk = reader.read(part, &[0]).expect("a part");
            chunk.validate_external().expect("every payload block checks out");
            assert_eq!(chunk.value_at(0, 0), Value::Varchar(value(part * per_part)));
        }

        // The last block is wherever the writer was when it was encoded, which the index says.
        let mut header = [0; DICTIONARY_HEADER];
        read_at(&reader.file, dictionary.offset, &mut header).expect("dictionary header");
        let count = u32::from_le_bytes(header[0..4].try_into().expect("four bytes")) as usize;
        let blocks = u32::from_le_bytes(header[8..12].try_into().expect("four bytes")) as usize;
        let width = u32::from_le_bytes(header[12..16].try_into().expect("four bytes"));
        let bits = (width & !DICTIONARY_FLAGS) as usize;
        let mut place = [0; 16];
        let at = DICTIONARY_HEADER + offset_bytes(count, bits) + (blocks - 1) * 16;
        read_at(&reader.file, dictionary.offset + at as u64, &mut place).expect("its place");
        let start = u64::from_le_bytes(place[..8].try_into().expect("eight bytes"));
        let length = u64::from_le_bytes(place[8..].try_into().expect("eight bytes"));
        let mut file = OpenOptions::new().write(true).open(&path).expect("open dictionary page");
        file.seek(SeekFrom::Start(start + length - 4)).expect("the last bytes of the last block");
        file.write_all(&[255]).expect("damage the last payload block");
        let reader = Reader::open(&path).expect("the directory and the index are untouched");
        let chunk = reader.read(parts - 1, &[0]).expect("the code page remains valid");
        let error = chunk.validate_external().expect_err("the damage must reach the caller");
        assert!(error.message().contains("payload checksum differs"), "{error}");
        fs::remove_file(path).expect("remove scratch file");
    }

    /// Values of different lengths read back where the offsets say they do.
    ///
    /// The offsets are packed at one width for the column, they are relative to the payload block a
    /// value lands in, and they go in runs of half a block, so there are two boundaries where the
    /// arithmetic could be off by one and neither shows up on values that are all the same length.
    /// This writes 5,000 values whose lengths cycle through a wide range and reads every one back,
    /// so the first value of a block, the last value of a run and the last value of a block are all
    /// covered several times over. An empty value is in the cycle because a zero length span is the
    /// case the reader short circuits.
    ///
    /// Six thousand rows over those 5,000 values, because a column the writer finds to be all
    /// distinct is written without a dictionary and then there are no packed offsets to be off by
    /// one in.
    #[test]
    fn values_of_different_lengths_read_back_out_of_packed_offsets() {
        let path = path("dictionary-offsets");
        let value = |row: usize| {
            let row = row % 5_000;
            if row % 511 == 3 { String::new() } else { "x".repeat(row % 97) + &format!("{row:05}") }
        };
        let rows = 6_000;
        let mut writer =
            Writer::create(&path, "items", vec![Field::required("text", LogicalType::Varchar)])
                .expect("new file");
        let values = (0..rows).map(|row| Value::Varchar(value(row))).collect::<Vec<_>>();
        for part in values.chunks(1_000) {
            let chunk =
                Chunk::new(vec![Vector::from_values(LogicalType::Varchar, part).expect("strings")])
                    .expect("matching rows");
            writer.append(&chunk).expect("a part");
        }
        writer.finish().expect("commit");

        let reader = Reader::open(&path).expect("reopen from disk");
        assert!(
            rows > TEXT_PAYLOAD_VALUES * 4,
            "the dictionary has to be several blocks for this to be testing anything"
        );
        for part in 0..rows / 1_000 {
            let chunk = reader.read(part, &[0]).expect("a part");
            for row in 0..1_000 {
                let row = part * 1_000 + row;
                assert_eq!(
                    chunk.value_at(row % 1_000, 0),
                    Value::Varchar(value(row)),
                    "value {row}"
                );
            }
        }
        // The lengths a vector at a time, twice over, because the first pass is what makes the
        // table of ends worth building and the second is read out of the lengths worked out of it.
        for _ in 0..2 {
            for part in 0..rows / 1_000 {
                let chunk = reader.read(part, &[0]).expect("a part");
                let mut lens = vec![0_i64; 1_000];
                let column = chunk.column(0).expect("one column");
                assert!(column.try_bytes_lens(&mut lens).expect("lengths"), "a stored column");
                for (row, &len) in lens.iter().enumerate() {
                    let row = part * 1_000 + row;
                    assert_eq!(len as usize, value(row).len(), "the length of value {row}");
                }
            }
        }
        fs::remove_file(path).expect("remove scratch file");
    }

    /// Lengths start again at every block, and ends that go backwards inside one give no table.
    #[test]
    fn lengths_restart_at_each_block_and_refuse_ends_that_go_backwards() {
        let mut ends: Vec<u32> = (1..=TEXT_PAYLOAD_VALUES as u32).map(|at| at * 2).collect();
        ends.extend([3, 3, 10]);
        let Some(Lengths::Narrow(lens)) = lengths_of(&ends) else { panic!("short ordered ends") };
        assert!(lens[..TEXT_PAYLOAD_VALUES].iter().all(|&len| len == 2));
        assert_eq!(&lens[TEXT_PAYLOAD_VALUES..], &[3, 0, 7]);
        // One value longer than sixteen bits keeps every length at four bytes.
        let long = [5, 70_005, 70_006];
        let Some(Lengths::Wide(lens)) = lengths_of(&long) else { panic!("long ordered ends") };
        assert_eq!(lens, [5, 70_000, 1]);
        let mut read = Vec::new();
        Lengths::Wide(lens).extend_at(&[1, 9, 0], &mut read);
        assert_eq!(read, [70_000, 0, 5], "a position past the end is no length");
        ends.push(9);
        assert!(lengths_of(&ends).is_none());
    }

    /// Every worker of a scan wants the dictionary at the same moment and one of them fetches it.
    ///
    /// Asking a `OnceLock` whether it holds something answers the question a worker that already has
    /// the dictionary is asking and not the one a worker without it is asking, which is whether
    /// somebody is already on their way with it. Sixteen workers that all miss will all read the
    /// page, all verify it and all decode it, and fifteen will drop the result. Nothing about that
    /// is incorrect, which is why it went unnoticed, and it showed up as ClickBench 38 getting
    /// slower when the scan in front of it got faster and stopped staggering the arrivals.
    ///
    /// The barrier is what makes the test about that rather than about luck. Without it the first
    /// thread is usually finished before the last one starts and the count is one either way.
    #[test]
    fn a_global_dictionary_is_opened_once_however_many_workers_ask_at_once() {
        let path = path("dictionary-once");
        let parts = 8;
        let per_part = 500;
        let value =
            |row: usize| format!("{row:07} a value long enough to be worth a payload block");
        let mut writer =
            Writer::create(&path, "items", vec![Field::required("text", LogicalType::Varchar)])
                .expect("new file");
        for part in 0..parts {
            let values = (0..per_part)
                .map(|row| Value::Varchar(value(part * per_part + row)))
                .collect::<Vec<_>>();
            let chunk = Chunk::new(vec![
                Vector::from_values(LogicalType::Varchar, &values).expect("strings"),
            ])
            .expect("matching rows");
            writer.append(&chunk).expect("a part");
        }
        writer.finish().expect("commit");

        let reader = Reader::open(&path).expect("reopen from disk");
        assert!(reader.table.dictionaries[0].is_some(), "the column has to have one to share");
        assert_eq!(reader.reads().dictionaries, 0, "opening the file does not open a dictionary");

        let workers = 16;
        let gate = std::sync::Barrier::new(workers);
        std::thread::scope(|scope| {
            for worker in 0..workers {
                let reader = reader.clone();
                let gate = &gate;
                scope.spawn(move || {
                    gate.wait();
                    let chunk = reader.read(worker % parts, &[0]).expect("a part");
                    assert_eq!(
                        chunk.value_at(0, 0),
                        Value::Varchar(value((worker % parts) * per_part))
                    );
                });
            }
        });

        assert_eq!(reader.reads().dictionaries, 1, "sixteen workers, one dictionary, one open");
        fs::remove_file(path).expect("remove scratch file");
    }

    /// The sorted order sits outside the index the page checksum covers, because a query that
    /// never searches a dictionary should not read it, so it carries its own checksums and this is
    /// what says they are checked. A search that trusted a damaged order would give a wrong answer
    /// rather than a slow one.
    #[test]
    fn a_damaged_sorted_order_is_an_error() {
        let path = path("damaged-order");
        let mut writer = Writer::create(
            &path,
            "items",
            vec![
                Field::required("id", LogicalType::Integer),
                Field::new("text", LogicalType::Varchar),
            ],
        )
        .expect("new file");
        writer.append(&sample()).expect("stripe written");
        writer.finish().expect("commit");

        let reader = Reader::open(&path).expect("valid directory");
        let page = reader.table.dictionaries[1].expect("string dictionary page");
        let mut header = [0; DICTIONARY_HEADER];
        read_at(&reader.file, page.offset, &mut header).expect("dictionary header");
        let index_len = dictionary_index_len(&header);
        let mut file = OpenOptions::new().write(true).open(&path).expect("open dictionary page");
        file.seek(SeekFrom::Start(page.offset + index_len)).expect("the first head");
        file.write_all(&[255]).expect("damage the order");

        let dictionary = reader.dictionary(1).expect("read").expect("a string column has one");
        let error = dictionary.compare_rank(0, b"anything").expect_err("a damaged order is caught");
        assert!(error.message().contains("rank checksum differs"), "{error}");
        fs::remove_file(path).expect("remove scratch file");
    }

    /// Codes stay in first appearance order and the sorted order is written beside them, so a
    /// reader can put the values back in order without the writer having had to know them all
    /// before it handed out the first code.
    #[test]
    fn a_global_dictionary_carries_the_sorted_order_of_its_values() {
        // Chosen so the sort cannot be decided on the first eight bytes alone. Three values share
        // a nine byte prefix, one is a prefix of another, and one is empty.
        let spellings = ["overlong1z", "b", "", "overlong1a", "overlong", "ab", "a", "overlong1"];
        let path = path("dictionary-order");
        let mut writer =
            Writer::create(&path, "items", vec![Field::new("text", LogicalType::Varchar)])
                .expect("new file");
        writer
            .append(
                &Chunk::new(vec![
                    Vector::from_values(
                        LogicalType::Varchar,
                        &spellings.map(|text| Value::Varchar(text.into())),
                    )
                    .expect("strings"),
                ])
                .expect("one column"),
            )
            .expect("stripe written");
        writer.finish().expect("commit");

        let reader = Reader::open(&path).expect("valid directory");
        let dictionary = reader.dictionary(0).expect("read").expect("a string column has one");
        let count = dictionary.ranks().expect("a v10 file stores one");
        assert_eq!(count, spellings.len(), "every distinct value has a rank");
        let order = (0..count)
            .map(|rank| dictionary.code_at_rank(rank).expect("a code"))
            .collect::<Vec<_>>();
        let mut seen = order.clone();
        seen.sort_unstable();
        assert_eq!(seen, (0..spellings.len() as u32).collect::<Vec<_>>(), "a permutation of codes");

        let ranked = order
            .iter()
            .map(|&code| {
                dictionary.try_bytes_at(code as usize).expect("read").expect("a value").to_vec()
            })
            .collect::<Vec<_>>();
        let mut expected = spellings.map(|text| text.as_bytes().to_vec()).to_vec();
        expected.sort();
        assert_eq!(ranked, expected, "rank order is value order");

        // What a search asks, on the values themselves rather than through a kernel, so that a
        // file whose heads disagree with its bytes is caught here rather than as a wrong answer.
        for (rank, value) in expected.iter().enumerate() {
            assert_eq!(
                dictionary.compare_rank(rank, value).expect("compare"),
                Ordering::Equal,
                "rank {rank} is its own value"
            );
            if rank > 0 {
                assert_eq!(
                    dictionary.compare_rank(rank - 1, value).expect("compare"),
                    Ordering::Less,
                    "rank {rank} follows the one before it"
                );
            }
        }
        fs::remove_file(path).expect("remove scratch file");
    }

    /// Five text columns of different sizes close at the same time, and each comes back with its
    /// own values in its own order.
    ///
    /// The sizes differ so that the columns are taken in an order that is not the column order, and
    /// the values of each column are spelled with its number so that one column's page written in
    /// another's place would read back as the wrong strings rather than the right ones by chance.
    #[test]
    fn text_columns_closed_at_once_each_keep_their_own_dictionary() {
        let sizes = [300_usize, 5_000, 40, 2_000, 1_200];
        let path = path("dictionaries-at-once");
        let fields = (0..sizes.len())
            .map(|column| Field::new(format!("text{column}"), LogicalType::Varchar))
            .collect::<Vec<_>>();
        let mut writer = Writer::create(&path, "items", fields).expect("new file");
        let rows = 10_000_usize;
        for start in (0..rows).step_by(1_024) {
            let columns = sizes
                .iter()
                .enumerate()
                .map(|(column, &size)| {
                    let values = (start..(start + 1_024).min(rows))
                        .map(|row| Value::Varchar(format!("c{column}-{:05}", (row * 7919) % size)))
                        .collect::<Vec<_>>();
                    Vector::from_values(LogicalType::Varchar, &values).expect("strings")
                })
                .collect::<Vec<_>>();
            writer.append(&Chunk::new(columns).expect("five columns")).expect("stripe written");
        }
        writer.finish().expect("commit");

        let reader = Reader::open(&path).expect("valid directory");
        for (column, &size) in sizes.iter().enumerate() {
            let dictionary =
                reader.dictionary(column).expect("read").expect("a string column has one");
            let count = dictionary.ranks().expect("a v10 file stores one");
            assert_eq!(count, size, "column {column} has its own distinct count");
            let ranked = (0..count)
                .map(|rank| {
                    let code = dictionary.code_at_rank(rank).expect("a code");
                    dictionary.try_bytes_at(code as usize).expect("read").expect("a value").to_vec()
                })
                .collect::<Vec<_>>();
            let expected = (0..size)
                .map(|value| format!("c{column}-{value:05}").into_bytes())
                .collect::<Vec<_>>();
            assert_eq!(ranked, expected, "column {column} ranks its own values in order");
        }
        fs::remove_file(path).expect("remove scratch file");
    }

    /// A dictionary large enough to be decoded and sorted on several threads ranks the way one small
    /// enough for one thread does.
    ///
    /// Seventy thousand values over sixty nine blocks, in no order and each four times over so the
    /// column is worth a dictionary, written and ranked in the close.
    /// Some share a long prefix and some differ only in the last byte, so the buckets of the sort cut
    /// through runs of values that agree for a long way.
    #[test]
    fn a_large_dictionary_ranks_in_value_order() {
        let path = path("dictionary-large-rank");
        let value = |row: u64| {
            let mixed = row.wrapping_mul(0x9e37_79b9_7f4a_7c15) >> 40;
            match row % 3 {
                0 => format!("https://example.com/a/long/shared/path/{mixed:08}"),
                1 => format!("{mixed}"),
                _ => format!("x{}", row % 1000).repeat(1 + (row % 4) as usize) + &row.to_string(),
            }
        };
        let distinct = 70_000;
        let parts = 4 * distinct / 1000;
        let per_part = 1000;
        let mut writer =
            Writer::create(&path, "items", vec![Field::required("text", LogicalType::Varchar)])
                .expect("new file");
        for part in 0..parts {
            let values = (0..per_part)
                .map(|row| Value::Varchar(value((part * per_part + row) / 4)))
                .collect::<Vec<_>>();
            let chunk = Chunk::new(vec![
                Vector::from_values(LogicalType::Varchar, &values).expect("strings"),
            ])
            .expect("matching rows");
            writer.append(&chunk).expect("a part");
        }
        writer.finish().expect("commit");

        let reader = Reader::open(&path).expect("reopen from disk");
        let dictionary = reader.dictionary(0).expect("read").expect("a string column has one");
        let count = dictionary.ranks().expect("a ranked dictionary");
        assert_eq!(count, distinct as usize, "every distinct value has a rank");
        assert!(count >= PARALLEL_SORT_MIN, "too few values to be sorted on more than one thread");
        let ranked = (0..count)
            .map(|rank| {
                let code = dictionary.code_at_rank(rank).expect("a code");
                dictionary.try_bytes_at(code as usize).expect("read").expect("a value").to_vec()
            })
            .collect::<Vec<_>>();
        let mut expected = (0..distinct).map(|row| value(row).into_bytes()).collect::<Vec<_>>();
        expected.sort();
        assert_eq!(ranked, expected, "rank order is value order");
        fs::remove_file(path).expect("remove scratch file");
    }

    /// A string column's synopsis is turned into values without keeping the blocks it went through.
    ///
    /// Three thousand values, every fifth of them four times over, so the synopsis is a prefix of
    /// five hundred and twelve codes spread over all three payload blocks. Reading it used to leave
    /// all three decoded for as long as the reader lived. It leaves none of them now, and the second
    /// read answers out of what the first remembered.
    /// A directory read out of the file a window at a time is the directory read whole.
    ///
    /// The windows here are far smaller than any field is long, so every kind of field is split
    /// across a refill somewhere, and a bound is offered to its codec short more than once. The
    /// synopses are left in the file, and each one read back from where it was left is the one the
    /// whole read decoded.
    #[test]
    fn a_directory_read_a_window_at_a_time_is_the_directory_read_whole() {
        let path = path("windowed-directory");
        let fields = vec![
            Field::required("id", LogicalType::BigInt),
            Field::required("word", LogicalType::Varchar),
            Field::new("score", LogicalType::Double),
        ];
        let mut writer = Writer::create(&path, "items", fields).expect("new file");
        for part in 0..70_i64 {
            let ids = (0..100).map(|row| Value::BigInt(part * 100 + row % 7)).collect::<Vec<_>>();
            let words = (0..100)
                .map(|row| Value::Varchar(format!("word {}", row % 13)))
                .collect::<Vec<_>>();
            let scores = (0..100)
                .map(|row| if row % 4 == 0 { Value::Null } else { Value::Double(row as f64) })
                .collect::<Vec<_>>();
            let chunk = Chunk::new(vec![
                Vector::from_values(LogicalType::BigInt, &ids).expect("integers"),
                Vector::from_values(LogicalType::Varchar, &words).expect("strings"),
                Vector::from_values(LogicalType::Double, &scores).expect("doubles"),
            ])
            .expect("three columns");
            writer.append(&chunk).expect("a part");
        }
        writer.finish().expect("commit");

        let catalog = Catalog::open(&path).expect("reopen");
        let entry = catalog.entries.first().expect("one table").directory;
        let (offset, length) = (entry.offset, entry.length as usize);
        let mut bytes = vec![0; length];
        read_at(&catalog.file, offset, &mut bytes).expect("the directory");
        assert_eq!(file_checksum(&catalog.file, offset, length).expect("checksum"), entry.hash);
        let whole = decode_directory(&bytes, catalog.size).expect("whole");
        assert!(whole.stripes.len() > 1, "the table should span stripes");
        for size in [1, 7, 33, 4_096] {
            let mut cursor = Cursor::over(&catalog.file, offset, length);
            cursor.window.as_mut().expect("a window").size = size;
            let windowed = read_directory(cursor, catalog.size, Some(offset)).expect("windowed");
            assert_eq!(format!("{:?}", windowed.stripes), format!("{:?}", whole.stripes));
            assert_eq!(format!("{:?}", windowed.fields), format!("{:?}", whole.fields));
            let mut stored = 0;
            for (column, (left, held)) in
                windowed.frequencies.iter().zip(&whole.frequencies).enumerate()
            {
                match (left, held) {
                    (None, None) => {}
                    (
                        Some(super::Frequencies::Stored { span, values }),
                        Some(super::Frequencies::Held(summary)),
                    ) => {
                        let mut one = vec![0; span.length as usize];
                        read_at(&catalog.file, span.offset, &mut one).expect("a synopsis");
                        let read = decode_summary(
                            &mut Cursor::new(&one),
                            &whole.fields[column],
                            whole.rows,
                            *values,
                        )
                        .expect("a valid synopsis")
                        .expect("one is there");
                        assert_eq!(format!("{read:?}"), format!("{summary:?}"));
                        stored += 1;
                    }
                    other => panic!("column {column} came back as {other:?}"),
                }
            }
            assert!(stored >= 2, "only {stored} synopses were left in the file");
        }
        let reader = catalog.table("items").expect("the table");
        assert!(reader.frequency_summaries[1].get().is_none());
        assert!(reader.top_frequencies(1, 1).expect("a readable synopsis").is_some());
        let first = reader.frequency_summaries[1].get().expect("decoded synopsis");
        let clone = reader.clone();
        assert!(clone.top_frequencies(1, 1).expect("cached synopsis").is_some());
        assert!(Arc::ptr_eq(first, clone.frequency_summaries[1].get().expect("same synopsis")));
        fs::remove_file(path).expect("remove scratch file");
    }

    #[test]
    fn a_checksum_carried_across_reads_is_the_checksum_of_the_whole() {
        let path = path("file-checksum");
        let bytes = (0..200_000_u32)
            .map(|at| (at.wrapping_mul(2_654_435_761) >> 13) as u8)
            .collect::<Vec<_>>();
        fs::write(&path, &bytes).expect("scratch file");
        let file = File::open(&path).expect("open");
        for (offset, length) in [
            (0, 0),
            (3, 1),
            (5, 31),
            (0, 32),
            (9, 33),
            (1, 65_536),
            (7, 65_567),
            (0, 200_000),
            (11, 131_101),
        ] {
            let whole = checksum(&bytes[offset..offset + length]);
            assert_eq!(
                file_checksum(&file, offset as u64, length).expect("read"),
                whole,
                "{offset} {length}"
            );
        }
        fs::remove_file(path).expect("remove scratch file");
    }

    #[test]
    fn a_string_synopsis_is_read_without_keeping_the_dictionary_blocks() {
        let path = path("synopsis-keeps-no-block");
        let spelled = |index: usize| Value::Varchar(format!("phrase {index:05}"));
        let mut values = (0..3_000).map(spelled).collect::<Vec<_>>();
        for _ in 0..3 {
            values.extend((0..3_000).step_by(5).map(spelled));
        }
        let mut writer =
            Writer::create(&path, "items", vec![Field::required("text", LogicalType::Varchar)])
                .expect("new file");
        for part in values.chunks(1_024) {
            writer
                .append(
                    &Chunk::new(vec![
                        Vector::from_values(LogicalType::Varchar, part).expect("strings"),
                    ])
                    .expect("one column"),
                )
                .expect("a part");
        }
        writer.finish().expect("commit");

        let reader = Reader::open(&path).expect("reopen from disk");
        let dictionary = reader.dictionary(0).expect("read").expect("a string column has one");
        let resting = dictionary.footprint();
        let prefix = reader.frequency_prefix(0).expect("a readable synopsis").expect("one");
        assert_eq!(prefix.entries.len(), 512);
        for (value, count) in &prefix.entries {
            let Value::Varchar(text) = value else { panic!("a string column gave {value:?}") };
            let index = text["phrase ".len()..].parse::<usize>().expect("a spelled number");
            assert_eq!((index % 5, *count), (0, 4), "{text} came back with {count}");
        }
        assert_eq!(dictionary.footprint(), resting, "reading the synopsis kept a decoded block");
        let again = reader.frequency_prefix(0).expect("a readable synopsis").expect("one");
        assert_eq!(again.entries, prefix.entries);
        fs::remove_file(path).expect("remove scratch file");
    }

    /// `length` over a stored column keeps a count a value rather than the blocks it counted.
    ///
    /// Reading the bytes a row at a time keeps every block it touches, so a scan of `length` over a
    /// whole column used to end up holding the column decoded. The counts are what is kept now, and
    /// they have to be the counts of characters rather than bytes, which is why the values here are
    /// not ASCII.
    #[test]
    fn character_lengths_are_counted_without_keeping_the_dictionary_blocks() {
        let path = path("character-lengths");
        let spellings = (0..2_500)
            .map(|index| Value::Varchar(format!("héllo {index:05} {}", "ü".repeat(index % 30))))
            .collect::<Vec<_>>();
        let mut writer =
            Writer::create(&path, "items", vec![Field::required("text", LogicalType::Varchar)])
                .expect("new file");
        for part in spellings.chunks(1_024) {
            writer
                .append(
                    &Chunk::new(vec![
                        Vector::from_values(LogicalType::Varchar, part).expect("strings"),
                    ])
                    .expect("one column"),
                )
                .expect("a part");
        }
        writer.finish().expect("commit");

        let reader = Reader::open(&path).expect("reopen from disk");
        let dictionary = reader.dictionary(0).expect("read").expect("a string column has one");
        let resting = dictionary.footprint();
        let mut lens = Vec::new();
        assert!(dictionary.try_chars_lens(&mut lens).expect("counted"), "a stored source counts");
        let counted = dictionary.footprint() - resting;
        let blocks = dictionary.len().div_ceil(TEXT_PAYLOAD_VALUES);
        assert!(
            counted <= blocks * TEXT_PAYLOAD_VALUES * size_of::<u32>(),
            "counting kept {counted} bytes, more than a count a value"
        );
        let expected = (0..dictionary.len())
            .map(|code| {
                let bytes = dictionary.try_bytes_at(code).expect("read").expect("a value");
                i64::try_from(std::str::from_utf8(bytes).expect("utf-8").chars().count())
                    .expect("small")
            })
            .collect::<Vec<_>>();
        assert_eq!(lens, expected, "a count is the number of characters, not of bytes");
        let mut again = Vec::new();
        assert!(dictionary.try_chars_lens(&mut again).expect("counted"));
        assert_eq!(again, lens, "the kept counts answer the second time");
        fs::remove_file(path).expect("remove scratch file");
    }

    /// Writes one column of strings whose code is where they sit in `spellings`, and reopens it.
    fn stored_spellings(label: &str, spellings: &[String]) -> (PathBuf, Reader) {
        let path = path(label);
        let values = spellings.iter().map(|text| Value::Varchar(text.clone())).collect::<Vec<_>>();
        let mut writer =
            Writer::create(&path, "items", vec![Field::new("text", LogicalType::Varchar)])
                .expect("new file");
        for part in values.chunks(1_024) {
            writer
                .append(
                    &Chunk::new(vec![
                        Vector::from_values(LogicalType::Varchar, part).expect("strings"),
                    ])
                    .expect("one column"),
                )
                .expect("a part");
        }
        writer.finish().expect("commit");
        let reader = Reader::open(&path).expect("reopen from disk");
        (path, reader)
    }

    /// Codes that go all over a dictionary of `len` values, and every seventh row null.
    ///
    /// The shape of a vector a scan hands out: its codes are in row order, which lands them in
    /// every block of the dictionary in no order at all, so a read of the whole vector has to put
    /// them in block order itself to read each block once.
    fn scattered_rows(len: usize) -> (Vec<u32>, Vec<bool>) {
        let codes = (0..len)
            .map(|row| u32::try_from(row * 7_919 % len).expect("a small dictionary"))
            .collect::<Vec<_>>();
        let valid = (0..len).map(|row| row % 7 != 3).collect::<Vec<_>>();
        (codes, valid)
    }

    /// `length` over a vector with nulls keeps the counts and not the blocks, the same as over one
    /// without.
    ///
    /// The whole vector count used to be taken only when no row was null, and every other vector
    /// went a row at a time through the bytes, which keeps every block it reads. A column with a
    /// null in each vector was held decoded after one `length` over it.
    #[test]
    fn character_lengths_with_nulls_are_counted_without_keeping_the_dictionary_blocks() {
        let spellings = (0..2_500)
            .map(|index| format!("héllo {index:05} {}", "ü".repeat(index % 30)))
            .collect::<Vec<_>>();
        let (path, reader) = stored_spellings("character-lengths-nulls", &spellings);
        let dictionary = reader.dictionary(0).expect("read").expect("a string column has one");
        let (codes, valid) = scattered_rows(spellings.len());
        let rows = Vector::dictionary_over(codes.clone(), Arc::clone(&dictionary))
            .expect("every code is inside")
            .with_validity(Validity::from_run(&valid));

        let resting = dictionary.footprint();
        let lens = rudb_kernels::call("length", &[&rows], &LogicalType::BigInt, None)
            .expect("length reads");
        let counted = dictionary.footprint() - resting;
        let blocks = dictionary.len().div_ceil(TEXT_PAYLOAD_VALUES);
        assert!(
            counted <= blocks * TEXT_PAYLOAD_VALUES * size_of::<u32>(),
            "length over a vector with nulls kept {counted} bytes, more than a count a value"
        );
        let expected = (0..rows.len())
            .map(|row| match valid[row] {
                true => Value::BigInt(
                    i64::try_from(spellings[codes[row] as usize].chars().count()).expect("small"),
                ),
                false => Value::Null,
            })
            .collect::<Vec<_>>();
        let answers = (0..lens.len()).map(|row| lens.value_at(row)).collect::<Vec<_>>();
        assert_eq!(answers, expected, "a count of characters where a row has one, null elsewhere");
        fs::remove_file(path).expect("remove scratch file");
    }

    /// `lower`, `upper` and `substring` read a stored dictionary a block at a time and keep none of
    /// it while the column is at its budget, until reading without keeping stops being cheap.
    ///
    /// The three used to read a row at a time through the bytes, which keeps every block a row lands
    /// in for as long as the table is open. They read the whole vector in one visit now, and the
    /// dictionary here is opened with a budget of zero so that what a visit would keep under the
    /// budget of a running database is what the test sees dropped. After a column's worth of blocks
    /// has been decoded and dropped the visit keeps what it reads, which is what bounds its cost on
    /// a scan whose codes keep coming back to every block, and the end of the test holds it to that.
    #[test]
    fn string_kernels_read_a_stored_dictionary_without_keeping_its_blocks() {
        let spellings = (0..2_500)
            .map(|index| format!("HéLLo {index:05} {}", "Üß".repeat(index % 30)))
            .collect::<Vec<_>>();
        let (path, reader) = stored_spellings("string-kernels", &spellings);
        let page = reader.table.dictionaries[0].expect("a string column has one");
        let starved =
            open_global_dictionary(Arc::clone(&reader.file), page, &LogicalType::Varchar, 0)
                .expect("a dictionary opens whatever it may keep");
        let starved = Arc::new(starved);
        let (codes, valid) = scattered_rows(spellings.len());
        let rows = Vector::dictionary_over(codes.clone(), Arc::clone(&starved))
            .expect("every code is inside")
            .with_validity(Validity::from_run(&valid));
        let expected = |each: &dyn Fn(&str) -> String| {
            (0..rows.len())
                .map(|row| match valid[row] {
                    true => Value::Varchar(each(&spellings[codes[row] as usize])),
                    false => Value::Null,
                })
                .collect::<Vec<_>>()
        };
        let answers =
            |vector: &Vector| (0..vector.len()).map(|row| vector.value_at(row)).collect::<Vec<_>>();

        // What a visit may add is the table of where every value ends, four bytes a value, which
        // reading every value this often makes worth building. A block is tens of bytes a value.
        let resting = starved.footprint();
        let ends = spellings.len() * size_of::<u32>();
        let lowered = rudb_kernels::call("lower", &[&rows], &LogicalType::Varchar, None)
            .expect("lower reads");
        assert_eq!(answers(&lowered), expected(&|text| text.to_lowercase()), "lower");
        assert!(starved.footprint() <= resting + ends, "lower kept a block it read");

        let start = Vector::constant(LogicalType::BigInt, Value::BigInt(3), rows.len());
        let length = Vector::constant(LogicalType::BigInt, Value::BigInt(9), rows.len());
        let cut =
            rudb_kernels::call("substring", &[&rows, &start, &length], &LogicalType::Varchar, None)
                .expect("substring reads");
        let cut_of = |text: &str| text.chars().skip(2).take(9).collect::<String>();
        assert_eq!(answers(&cut), expected(&cut_of), "substring");
        assert!(starved.footprint() <= resting + ends, "substring kept a block it read");

        // Every block has been read twice now and dropped the second time as well, which is a
        // column's worth dropped for want of a budget, so the next visit keeps what it reads.
        let raised = rudb_kernels::call("upper", &[&rows], &LogicalType::Varchar, None)
            .expect("upper reads");
        assert_eq!(answers(&raised), expected(&|text| text.to_uppercase()), "upper");
        let payload = spellings.iter().map(String::len).sum::<usize>();
        assert!(
            starved.footprint() >= resting + payload,
            "a visit that has dropped a column's worth of blocks keeps what it reads"
        );
        let again = rudb_kernels::call("upper", &[&rows], &LogicalType::Varchar, None)
            .expect("upper reads kept blocks");
        assert_eq!(answers(&again), answers(&raised), "the kept blocks answer the same");
        fs::remove_file(path).expect("remove scratch file");
    }

    /// A sweep of the dictionary reads every value, and the second sweep keeps what it read, up to
    /// the budget.
    ///
    /// The point of the sweep is the resident size rather than the answer, so both are checked
    /// here. The first sweep keeps nothing, because a process that runs one statement never reads
    /// a block twice. A dictionary this small is well under [`TEXT_KEEP_BUDGET`], so the second
    /// sweep keeps everything and a third decodes nothing, which is what makes a session asking the
    /// same question again cost what it should. The ceiling is the other half of it and it has its own
    /// test below, because a ceiling that never binds is not a ceiling anybody checked.
    #[test]
    fn a_dictionary_sweep_reads_every_value_and_keeps_it_under_the_budget() {
        let path = path("dictionary-sweep");
        // Two thousand five hundred distinct values is two whole payload blocks and a part of a
        // third, so the sweep has to be called more than once and the last call has to stop short.
        let spellings = (0..2_500)
            .map(|index| Value::Varchar(format!("value {index:08} {}", "x".repeat(index % 40))))
            .collect::<Vec<_>>();
        let mut writer =
            Writer::create(&path, "items", vec![Field::new("text", LogicalType::Varchar)])
                .expect("new file");
        // A chunk is a part and a part is at most 1,024 rows, so the values go in three of them.
        // The dictionary is table wide and does not care where a value was written.
        for part in spellings.chunks(1_024) {
            writer
                .append(
                    &Chunk::new(vec![
                        Vector::from_values(LogicalType::Varchar, part).expect("strings"),
                    ])
                    .expect("one column"),
                )
                .expect("stripe written");
        }
        writer.finish().expect("commit");

        let reader = Reader::open(&path).expect("valid directory");
        let dictionary = reader.dictionary(0).expect("read").expect("a string column has one");
        assert_eq!(dictionary.len(), spellings.len(), "every value is distinct");
        for first in [0, TEXT_PAYLOAD_VALUES, TEXT_PAYLOAD_VALUES * 2] {
            assert!(dictionary.text_block_might_contain(first, b"value").expect("signature"));
            assert!(!dictionary.text_block_might_contain(first, b"google").expect("signature"));
        }

        let resting = dictionary.footprint();
        let sweep = || {
            let mut swept: Vec<Vec<u8>> = Vec::new();
            let mut at = 0;
            let mut calls = 0;
            while at < dictionary.len() {
                let stopped = dictionary
                    .sweep_text(at, dictionary.len(), &mut |index: usize, text: &[u8]| {
                        assert_eq!(index, swept.len(), "a sweep hands its values over in order");
                        swept.push(text.to_vec());
                        Ok(())
                    })
                    .expect("a sweep reads");
                assert!(stopped > at, "a sweep moves");
                at = stopped;
                calls += 1;
            }
            assert_eq!(calls, 3, "a sweep hands over one block at a time");
            swept
        };
        let swept = sweep();
        assert_eq!(dictionary.footprint(), resting, "a first sweep keeps nothing it decoded");
        assert_eq!(sweep(), swept, "a second sweep reads what the first did");
        let after = dictionary.footprint();
        assert!(after > resting, "a second sweep under the budget keeps what it decoded");

        let read = (0..dictionary.len())
            .map(|code| dictionary.try_bytes_at(code).expect("read").expect("a value").to_vec())
            .collect::<Vec<_>>();
        assert_eq!(swept, read, "a sweep answers what a point read answers");
        // A read per value is about what makes the unpacked ends worth building, so whether they
        // are built here depends on how many reads the sweep made on the way. They are the one thing
        // allowed to grow, by four bytes a value, and nothing of the payload is.
        let grown = dictionary.footprint() - after;
        assert!(
            grown == 0 || grown == dictionary.len() * size_of::<u32>(),
            "a point read of a kept block decodes nothing, and {grown} bytes grew"
        );
        fs::remove_file(path).expect("remove scratch file");
    }

    #[test]
    fn a_narrow_signature_of_an_older_file_answers_by_its_own_width() {
        let path = path("narrow-substring-signature");
        let blocks = [&b"https://google.com/"[..], b"https://example.org/", b"mail.google.com"];
        let mut grams = Vec::new();
        for text in blocks {
            let mut bits = vec![0_u8; NARROW_GRAM_BYTES];
            for gram in text.windows(4) {
                for bit in gram_bits(gram, NARROW_GRAM_BYTES) {
                    bits[bit / 8] |= 1 << (bit % 8);
                }
            }
            grams.extend(bits);
        }
        fs::write(&path, &grams).expect("scratch file");
        let file = File::open(&path).expect("open scratch file");
        let signatures = NativeGrams {
            start: 0,
            length: grams.len(),
            width: NARROW_GRAM_BYTES,
            hash: checksum(&grams),
            verdicts: Mutex::new(Vec::new()),
        };
        let verdict = signatures.verdicts(&file, b"google").expect("signatures read");
        assert_eq!(&verdict[..], &[true, false, true], "one verdict a block, at the narrow width");
        assert!(signatures.footprint() > 0, "a verdict is remembered");
        let again = signatures.verdicts(&file, b"google").expect("remembered");
        assert!(Arc::ptr_eq(&verdict, &again), "a second question about a literal reads nothing");

        let damaged = NativeGrams {
            hash: signatures.hash ^ 1,
            verdicts: Mutex::new(Vec::new()),
            ..signatures
        };
        let error = damaged.verdicts(&file, b"google").expect_err("a damaged region is refused");
        assert!(error.to_string().contains("substring signatures checksum differs"), "{error}");
        fs::remove_file(path).expect("remove scratch file");
    }

    #[test]
    fn a_damaged_substring_signature_is_checked_only_when_used() {
        let path = path("damaged-substring-signature");
        let mut writer =
            Writer::create(&path, "items", vec![Field::new("text", LogicalType::Varchar)])
                .expect("new file");
        let rows = [Value::Varchar("google".into()), Value::Varchar("example".into())];
        writer
            .append(
                &Chunk::new(vec![
                    Vector::from_values(LogicalType::Varchar, &rows).expect("strings"),
                ])
                .expect("one column"),
            )
            .expect("stripe written");
        writer.finish().expect("commit");

        let reader = Reader::open(&path).expect("valid directory");
        let page = reader.table.dictionaries[0].expect("string dictionary page");
        let mut file = OpenOptions::new().write(true).open(&path).expect("open dictionary page");
        file.seek(SeekFrom::Start(page.offset + u64::from(page.length) - 1))
            .expect("last signature byte");
        file.write_all(&[255]).expect("damage signature");
        let reader = Reader::open(&path).expect("the directory is still valid");
        let dictionary = reader.dictionary(0).expect("index is still valid").expect("dictionary");
        let error = dictionary
            .text_block_might_contain(0, b"goog")
            .expect_err("a used signature checks its own checksum");
        assert!(error.message().contains("substring signatures checksum differs"), "{error}");
        fs::remove_file(path).expect("remove scratch file");
    }

    /// A sweep over a block whose second run of offsets is short reads the same values as a point
    /// read does.
    ///
    /// The sweep decodes the offsets of a whole run at a time rather than a value at a time, and a
    /// run holds half a block, so the count it asks for is the run length everywhere but at the end
    /// of the dictionary. Two thousand five hundred values, which is what the test above writes,
    /// never puts a short run second in its block: the last block there begins on a run boundary and
    /// holds one run. Two thousand eight hundred does, so the last block is a whole run of five
    /// hundred and twelve followed by two hundred and forty, and an off by one in either the count
    /// asked for or the slice taken out of the answer shows up as a wrong value or a refusal.
    #[test]
    fn a_sweep_over_a_block_with_a_short_second_run_reads_what_a_point_read_reads() {
        let path = path("dictionary-sweep-short-run");
        let spellings = (0..2_800)
            .map(|index| Value::Varchar(format!("value {index:08} {}", "x".repeat(index % 40))))
            .collect::<Vec<_>>();
        let mut writer =
            Writer::create(&path, "items", vec![Field::new("text", LogicalType::Varchar)])
                .expect("new file");
        for part in spellings.chunks(1_024) {
            writer
                .append(
                    &Chunk::new(vec![
                        Vector::from_values(LogicalType::Varchar, part).expect("strings"),
                    ])
                    .expect("one column"),
                )
                .expect("stripe written");
        }
        writer.finish().expect("commit");

        let reader = Reader::open(&path).expect("valid directory");
        let dictionary = reader.dictionary(0).expect("read").expect("a string column has one");
        assert_eq!(dictionary.len(), spellings.len(), "every value is distinct");
        let last = dictionary.len() % TEXT_PAYLOAD_VALUES;
        assert!(last > TEXT_OFFSET_RUN, "the last block has to reach into a second run of offsets");
        assert!(last < TEXT_PAYLOAD_VALUES, "and that second run has to be short of a whole one");

        let mut swept: Vec<Vec<u8>> = Vec::new();
        let mut at = 0;
        while at < dictionary.len() {
            let stopped = dictionary
                .sweep_text(at, dictionary.len(), &mut |index: usize, text: &[u8]| {
                    assert_eq!(index, swept.len(), "a sweep hands its values over in order");
                    swept.push(text.to_vec());
                    Ok(())
                })
                .expect("a sweep reads");
            assert!(stopped > at, "a sweep moves");
            at = stopped;
        }
        let read = (0..dictionary.len())
            .map(|code| dictionary.try_bytes_at(code).expect("read").expect("a value").to_vec())
            .collect::<Vec<_>>();
        assert_eq!(swept, read, "a sweep answers what a point read answers");
        fs::remove_file(path).expect("remove scratch file");
    }

    /// The unpacked ends answer what the packed ends answer, on both sides of the switch.
    ///
    /// A column asked for one offset at a time reads them out of the packed form until the reads
    /// are worth a table and out of the table after that, so every value here is read twice and the
    /// two passes are compared against the spellings and against each other. Two thousand eight
    /// hundred values is two payload blocks and a bit, which puts the switch in the middle of the
    /// first pass and means the pass straddles a block boundary, where the start of a value is zero
    /// rather than the end of the value before it.
    #[test]
    fn the_unpacked_ends_answer_what_the_packed_ends_answer() {
        let path = path("dictionary-unpacked-ends");
        let spellings = (0..2_800)
            .map(|index| Value::Varchar(format!("value {index:08} {}", "x".repeat(index % 40))))
            .collect::<Vec<_>>();
        let mut writer =
            Writer::create(&path, "items", vec![Field::new("text", LogicalType::Varchar)])
                .expect("new file");
        for part in spellings.chunks(1_024) {
            writer
                .append(
                    &Chunk::new(vec![
                        Vector::from_values(LogicalType::Varchar, part).expect("strings"),
                    ])
                    .expect("one column"),
                )
                .expect("stripe written");
        }
        writer.finish().expect("commit");

        let reader = Reader::open(&path).expect("valid directory");
        let dictionary = reader.dictionary(0).expect("read").expect("a string column has one");
        assert_eq!(dictionary.len(), spellings.len(), "every value is distinct");
        let wanted = (0..spellings.len())
            .map(|index| format!("value {index:08} {}", "x".repeat(index % 40)).into_bytes())
            .collect::<Vec<_>>();

        let pass = |what: &str| {
            for (index, value) in wanted.iter().enumerate() {
                let len = dictionary.try_bytes_len_at(index).expect("read").expect("a value");
                assert_eq!(len, value.len(), "{what} has the wrong length at {index}");
                let bytes = dictionary.try_bytes_at(index).expect("read").expect("a value");
                assert_eq!(bytes, value.as_slice(), "{what} has the wrong value at {index}");
            }
        };
        pass("the first pass");
        pass("the second pass");

        // The whole vector in one call, over the text and through codes into it, which is how a
        // scan of a stored column hands it out. The codes run backwards and repeat so that they are
        // neither the positions nor in order.
        let lens = wanted.iter().map(|value| value.len() as i64).collect::<Vec<_>>();
        let mut whole = vec![0i64; wanted.len()];
        assert!(dictionary.try_bytes_lens(&mut whole).expect("read"), "the text answers whole");
        assert_eq!(whole, lens, "a vector of lengths answers what a length at a time answers");
        let codes = (0..4_000_u32).map(|row| (7 * (4_000 - row)) % 2_800).collect::<Vec<_>>();
        let coded = Vector::dictionary_over(codes.clone(), dictionary).expect("codes in range");
        let mut through = vec![0i64; codes.len()];
        assert!(coded.try_bytes_lens(&mut through).expect("read"), "the codes answer whole");
        for (row, &code) in codes.iter().enumerate() {
            assert_eq!(through[row], lens[code as usize], "row {row} reads code {code}");
            let one = coded.try_bytes_len_at(row).expect("read").expect("a value");
            assert_eq!(through[row], one as i64, "row {row} a row at a time");
        }

        // A handful of codes over a column nobody has read yet is short of the table, so the same
        // call answers out of the packed ends instead, and has to answer the same.
        let fresh = Reader::open(&path).expect("valid directory");
        let untouched = fresh.dictionary(0).expect("read").expect("a string column has one");
        let few = vec![2_799_u32, 0, 1_024, 1_023, 511, 512];
        let coded = Vector::dictionary_over(few.clone(), untouched).expect("in range");
        let mut short = vec![0i64; few.len()];
        assert!(coded.try_bytes_lens(&mut short).expect("read"), "the codes answer whole");
        let expected = few.iter().map(|&code| lens[code as usize]).collect::<Vec<_>>();
        assert_eq!(short, expected, "the packed ends answer what the table answers");
        fs::remove_file(path).expect("remove scratch file");
    }

    /// Narrowing a page takes what fits and refuses the page for anything that does not.
    ///
    /// The edges of the range on both sides and one step past each of them, for every type, because
    /// checking a page separately from converting it is only right if the check refuses exactly what
    /// `TryFrom` would have refused, and off by one there is a file that reads back a different
    /// number than it was given. The check is a bit pattern rather than a comparison, so it is not
    /// the shape a reader would guess from the bounds, which is why all six are here. The empty page
    /// is here because a check written the obvious way starts with the extremes the wrong way round
    /// and refuses it.
    #[test]
    fn narrowing_a_page_takes_what_fits_and_refuses_what_does_not() {
        assert_eq!(fit::<i8>(&[]).expect("an empty page fits anything"), Vec::<i8>::new());
        assert_eq!(fit::<i8>(&[-128, 0, 127]).expect("the edges fit"), vec![-128_i8, 0, 127]);
        fit::<i8>(&[128]).expect_err("one past the top does not fit");
        fit::<i8>(&[-129]).expect_err("one past the bottom does not fit");
        assert_eq!(fit::<u8>(&[0, 255]).expect("the edges fit"), vec![0_u8, 255]);
        fit::<u8>(&[256]).expect_err("one past the top does not fit");
        fit::<u8>(&[-1]).expect_err("a negative does not fit an unsigned page");
        assert_eq!(
            fit::<i16>(&[-32_768, 0, 32_767]).expect("the edges fit"),
            vec![-32_768_i16, 0, 32_767]
        );
        fit::<i16>(&[32_768]).expect_err("one past the top does not fit");
        fit::<i16>(&[-32_769]).expect_err("one past the bottom does not fit");
        assert_eq!(fit::<u16>(&[0, 65_535]).expect("the edges fit"), vec![0_u16, 65_535]);
        fit::<u16>(&[65_536]).expect_err("one past the top does not fit");
        fit::<u16>(&[-1]).expect_err("a negative does not fit an unsigned page");
        assert_eq!(
            fit::<i32>(&[i64::from(i32::MIN), 0, i64::from(i32::MAX)]).expect("the edges fit"),
            vec![i32::MIN, 0, i32::MAX]
        );
        fit::<i32>(&[i64::from(i32::MAX) + 1]).expect_err("one past the top does not fit");
        fit::<i32>(&[i64::from(i32::MIN) - 1]).expect_err("one past the bottom does not fit");
        assert_eq!(
            fit::<u32>(&[0, 4_294_967_295]).expect("the edges fit"),
            vec![0_u32, 4_294_967_295]
        );
        fit::<u32>(&[4_294_967_296]).expect_err("one past the top does not fit");
        fit::<u32>(&[-1]).expect_err("a negative does not fit an unsigned page");

        // One value in a page that fits is still a page that does not, which is the thing an or
        // into an accumulator could get wrong in a way a page of one value would never show.
        fit::<i8>(&[0, 1, 2, 128, 3]).expect_err("one bad value spoils the page");
    }

    /// The residue says yes to exactly what `TryFrom` says yes to.
    ///
    /// The edges above are the cases anyone would think to write down. This is the argument that
    /// there are no others, made by asking both questions about every value either narrow type could
    /// have an opinion about, and then about the values around the wide edges and the ends of an
    /// `i64`, which a range that size cannot reach.
    #[test]
    fn the_residue_agrees_with_a_checked_conversion_everywhere() {
        for value in -70_000_i64..70_000 {
            assert_eq!(fit::<i8>(&[value]).is_ok(), i8::try_from(value).is_ok(), "{value} as i8");
            assert_eq!(fit::<u8>(&[value]).is_ok(), u8::try_from(value).is_ok(), "{value} as u8");
            assert_eq!(fit::<i16>(&[value]).is_ok(), i16::try_from(value).is_ok(), "{value} i16");
            assert_eq!(fit::<u16>(&[value]).is_ok(), u16::try_from(value).is_ok(), "{value} u16");
        }
        let wide = [i64::MIN, i64::MIN + 1, i64::from(i32::MIN), 0, i64::from(u32::MAX), i64::MAX];
        for edge in wide {
            for step in -2_i64..=2 {
                let value = edge.saturating_add(step);
                assert_eq!(
                    fit::<i32>(&[value]).is_ok(),
                    i32::try_from(value).is_ok(),
                    "{value} as i32"
                );
                assert_eq!(
                    fit::<u32>(&[value]).is_ok(),
                    u32::try_from(value).is_ok(),
                    "{value} as u32"
                );
            }
        }
    }

    /// All three block layouts come back as the same values in the same order.
    ///
    /// Blocks outside the page are what every file this build writes holds. Blocks that say where
    /// they are but sit inside the page behind the order are format 26, and blocks behind one
    /// another with only their ends recorded are older still. Nothing in the writer produces the
    /// last two any more, so the only way to find out whether the reader still understands those
    /// files is to write them here. The
    /// bytes go straight into a file with no directory around them, because what is under test is
    /// [`open_global_dictionary`], which is handed a page and a file and asks the directory for
    /// nothing.
    ///
    /// Three thousand values so that there are three payload blocks and a partial fourth, which is
    /// what makes the last block the one place where a length and an end disagree about what they
    /// are counting.
    #[test]
    fn a_dictionary_reads_the_same_whether_its_blocks_say_where_they_are() {
        let spellings = (0..3_000)
            .map(|index| format!("value {index:08} {}", "y".repeat(index % 40)))
            .collect::<Vec<_>>();
        let mut read = Vec::new();
        for layout in ["outside", "inside", "behind"] {
            let mut dictionary = GlobalDictionary::new();
            for text in &spellings {
                dictionary.code(text).expect("a code for every spelling");
            }
            dictionary.finish_blocks().expect("the last block encodes");
            let order = dictionary.ranked(None).expect("a sorted order");
            // Where the blocks go if they start at `from` and follow one another.
            let laid = |from: u64| {
                let mut at = from;
                dictionary
                    .blocks
                    .iter()
                    .map(|block| {
                        let place =
                            Placed { start: at, length: block.len() as u64, hash: checksum(block) };
                        at += block.len() as u64;
                        place
                    })
                    .collect::<Vec<_>>()
            };
            let payload = dictionary.blocks.concat();
            let scattered = layout != "behind";
            let (bytes, encoded, offset, length) = if layout == "outside" {
                let mut bytes = vec![0; HEADER as usize];
                bytes.extend_from_slice(&payload);
                let encoded = encode_global_dictionary(&dictionary, &order, &laid(HEADER), true)
                    .expect("an encoding");
                let offset = bytes.len() as u64;
                bytes.extend_from_slice(&encoded.index);
                bytes.extend_from_slice(&encoded.ranks);
                bytes.extend_from_slice(&encoded.grams);
                let length = encoded.index.len() + encoded.ranks.len() + encoded.grams.len();
                (bytes, encoded, offset, length)
            } else {
                // The index is the same length wherever the blocks are, so a first pass says where
                // the page ends and the second writes the places that follow it.
                let first = encode_global_dictionary(&dictionary, &order, &laid(0), scattered)
                    .expect("an encoding");
                let body = (first.index.len() + first.ranks.len() + first.grams.len()) as u64;
                let encoded = encode_global_dictionary(&dictionary, &order, &laid(body), scattered)
                    .expect("an encoding");
                let mut bytes = encoded.index.clone();
                bytes.extend_from_slice(&encoded.ranks);
                bytes.extend_from_slice(&encoded.grams);
                bytes.extend_from_slice(&payload);
                let length = bytes.len();
                (bytes, encoded, 0, length)
            };
            let path = path(&format!("blocks-{layout}"));
            fs::write(&path, &bytes).expect("the dictionary is written on its own");
            let file = Arc::new(File::open(&path).expect("it opens again"));
            let page = Page {
                offset,
                length: u32::try_from(length).expect("a test dictionary is small"),
                hash: checksum(&encoded.index),
            };
            let opened =
                open_global_dictionary(file, page, &LogicalType::Varchar, TEXT_KEEP_BUDGET)
                    .expect("a dictionary laid out either way opens");
            let mut swept: Vec<Vec<u8>> = Vec::new();
            let mut at = 0;
            while at < opened.len() {
                at = opened
                    .sweep_text(at, opened.len(), &mut |_index: usize, text: &[u8]| {
                        swept.push(text.to_vec());
                        Ok(())
                    })
                    .expect("a sweep reads");
            }
            fs::remove_file(&path).expect("clean up");
            read.push(swept);
        }
        let wanted =
            spellings.iter().map(|text| text.as_bytes().to_vec()).collect::<Vec<Vec<u8>>>();
        assert_eq!(read[0], wanted, "the blocks outside the page hold the values");
        assert_eq!(read[1], read[0], "the blocks inside the page hold the same values");
        assert_eq!(read[2], read[0], "the blocks behind one another hold the same values");
    }

    /// A dictionary at its budget sweeps without keeping, and still answers what it answered.
    ///
    /// The budget is a quarter of a gigabyte in a running database, which is a fine size for a real
    /// column and no size at all for a test, so this opens the same dictionary a second time with a
    /// budget of zero. That is the shape of the hundred million row case: `URL` fills the budget
    /// somewhere in the middle of itself and everything past that point is read and dropped, which
    /// costs the decode again and holds none of it.
    #[test]
    fn a_dictionary_at_its_budget_sweeps_without_keeping() {
        let path = path("dictionary-budget");
        let spellings = (0..2_500)
            .map(|index| Value::Varchar(format!("value {index:08} {}", "y".repeat(index % 40))))
            .collect::<Vec<_>>();
        let mut writer =
            Writer::create(&path, "items", vec![Field::new("text", LogicalType::Varchar)])
                .expect("new file");
        for part in spellings.chunks(1_024) {
            writer
                .append(
                    &Chunk::new(vec![
                        Vector::from_values(LogicalType::Varchar, part).expect("strings"),
                    ])
                    .expect("one column"),
                )
                .expect("stripe written");
        }
        writer.finish().expect("commit");

        let reader = Reader::open(&path).expect("valid directory");
        let page = reader.table.dictionaries[0].expect("a string column has one");
        let file = Arc::clone(&reader.file);
        let starved = open_global_dictionary(file, page, &LogicalType::Varchar, 0)
            .expect("a dictionary opens whatever it may keep");

        let resting = starved.footprint();
        let mut swept: Vec<Vec<u8>> = Vec::new();
        let mut at = 0;
        while at < starved.len() {
            at = starved
                .sweep_text(at, starved.len(), &mut |_index: usize, text: &[u8]| {
                    swept.push(text.to_vec());
                    Ok(())
                })
                .expect("a sweep reads");
        }
        assert_eq!(swept.len(), spellings.len(), "a starved sweep still reads every value");
        assert_eq!(starved.footprint(), resting, "and keeps no block it decoded");

        let generous = reader.dictionary(0).expect("read").expect("a string column has one");
        let read = (0..generous.len())
            .map(|code| generous.try_bytes_at(code).expect("read").expect("a value").to_vec())
            .collect::<Vec<_>>();
        assert_eq!(swept, read, "a starved sweep answers what a point read answers");
        fs::remove_file(path).expect("remove scratch file");
    }

    #[test]
    fn damaged_membership_cannot_skip_a_string_page() {
        let path = path("damaged-membership");
        let mut writer = Writer::create(
            &path,
            "items",
            vec![
                Field::required("id", LogicalType::Integer),
                Field::new("text", LogicalType::Varchar),
            ],
        )
        .expect("new file");
        writer.append(&sample()).expect("stripe written");
        writer.finish().expect("commit");

        let reader = Reader::open(&path).expect("valid directory");
        let membership = reader.table.stripes[0].memberships.get(1).expect("string membership");
        let mut file = OpenOptions::new().write(true).open(&path).expect("open membership page");
        file.seek(SeekFrom::Start(membership.offset)).expect("membership start");
        file.write_all(&[255]).expect("damage membership");
        let error = reader.skips_codes(0, 1, &[3]).expect_err("corruption must not skip rows");
        assert!(error.message().contains("membership page checksum differs"), "{error}");
        fs::remove_file(path).expect("remove scratch file");
    }

    #[test]
    fn membership_delta_stream_is_sorted_exact_and_bounded() {
        let unique = unique_codes(&[900, 4, 4, 72, 9, u32::MAX]);
        assert_eq!(unique, [4, 9, 72, 900, u32::MAX]);
        let encoded = encode_membership(&unique);
        assert_eq!(
            decode_membership(&encoded).expect("valid membership"),
            [4, 9, 72, 900, u32::MAX]
        );
        // A stripe's index is the union of its parts', so a code in two of them is in it once and
        // the result is still one ascending run of deltas.
        let merged = merged_codes(vec![vec![4, 900], vec![9, 900, u32::MAX], vec![72]]);
        assert_eq!(merged, [4, 9, 72, 900, u32::MAX]);
        assert_eq!(
            decode_membership(&encode_membership(&merged)).expect("valid membership"),
            unique
        );
        assert!(decode_membership(&[1, 0x80]).is_err(), "a truncated varint is invalid");
        assert!(
            decode_membership(&[1, 0xff, 0xff, 0xff, 0xff, 0x10]).is_err(),
            "a value past u32 is invalid"
        );
    }

    #[test]
    fn a_global_dictionary_may_be_larger_than_one_column_page() {
        let dictionary = Page {
            offset: HEADER,
            length: u32::try_from(MAX_PAGE + 1).expect("the page bound fits on disk"),
            hash: 0,
        };
        let table = Table {
            name: "items".to_owned(),
            fields: vec![Field::new("text", LogicalType::Varchar)],
            stripes: Vec::new(),
            rows: 0,
            dictionaries: vec![Some(dictionary)],
            dictionary_payloads: Vec::new(),
            demoted: Vec::new(),
            distincts: vec![None],
            frequencies: vec![None],
            pair_frequencies: Vec::new(),
            frequency_texts: Vec::new(),
            host_groups: None,
            clustering: None,
            generation: 1,
            sections: Vec::new(),
        };
        let directory = encode_directory(&table).expect("directory");
        let file_size = dictionary.offset + u64::from(dictionary.length) + 1;

        let decoded = decode_directory(&directory, file_size).expect("large lazy dictionary");
        assert_eq!(decoded.dictionaries[0].expect("dictionary").length, dictionary.length);
    }

    #[test]
    fn a_column_with_one_value_everywhere_costs_almost_nothing_a_row() {
        let path = path("constant-codes");
        let mut writer =
            Writer::create(&path, "items", vec![Field::new("text", LogicalType::Varchar)])
                .expect("new file");
        let empty = vec![Value::Varchar(String::new()); 1024];
        for _ in 0..4 {
            let column = Vector::from_values(LogicalType::Varchar, &empty).expect("strings");
            writer.append(&Chunk::new(vec![column]).expect("one column")).expect("a part");
        }
        writer.finish().expect("commit");

        let reader = Reader::open(&path).expect("valid directory");
        let pages = reader.layout().columns.first().expect("one column").pages;
        // This column used to cost four bytes a row, 16,384 of them, the same as a column of four
        // thousand distinct URLs would. The cascade calls each part a constant, so what is left is
        // a tag, a count and the value, and the row count stops being what drives the number.
        assert!(pages < 256, "{pages} bytes of pages for 4,096 rows of one value");
        let read = reader.read(3, &[0]).expect("the last part back");
        assert_eq!(read.value_at(0, 0), Value::Varchar(String::new()));
        assert_eq!(read.value_at(1023, 0), Value::Varchar(String::new()));
        fs::remove_file(path).expect("remove scratch file");
    }

    #[test]
    fn a_cascade_value_too_wide_for_its_column_is_refused_rather_than_cut() {
        // What a damaged page looks like from here: the cascade decoded, so the bytes are not
        // truncated, but the values do not belong to the column the directory says they do.
        let over = vec![i64::from(i32::MAX) + 1];
        let error = narrowed(&LogicalType::Integer, over).expect_err("a page that disagrees");
        assert!(format!("{error}").contains("not of its type"), "{error}");
        assert!(narrowed(&LogicalType::BigInt, vec![i64::MIN]).is_ok(), "bigint holds all of i64");
        assert!(narrowed(&LogicalType::Varchar, vec![0]).is_err(), "strings are not integers");
    }

    #[test]
    fn a_code_stream_the_cascade_cannot_shrink_is_left_alone() {
        // A shift register rather than a run, because an arithmetic run is the one wide shape the
        // cascade does shrink. This is what a column with tens of millions of distinct values hands
        // over: full width codes with no order to them.
        let mut state: u32 = 0x9e37_79b9;
        let spread: Vec<u32> = (0..1024)
            .map(|_| {
                state ^= state << 13;
                state ^= state >> 17;
                state ^= state << 5;
                state
            })
            .collect();
        assert_eq!(encoded_codes(&spread).expect("no failure"), None);
        let near: Vec<u32> = (0..1024).collect();
        let coded = encoded_codes(&near).expect("no failure").expect("counting up is packable");
        assert!(coded.len() < near.len() * 4, "{} bytes for a run of 1,024", coded.len());
    }

    /// The columns of a stripe are encoded on whichever thread got to them, so the one thing that
    /// must not depend on which thread that was is the file. Two writes of the same rows are
    /// compared byte for byte rather than value for value, because a dictionary that two columns
    /// somehow shared would still read back correctly and would hand out its codes in the order the
    /// threads happened to run in, which is exactly what this is here to catch.
    #[test]
    fn two_writes_of_the_same_rows_give_the_same_bytes() {
        fn written(path: &PathBuf) {
            let fields = (0..40)
                .map(|column| {
                    let ty =
                        if column % 4 == 0 { LogicalType::Varchar } else { LogicalType::BigInt };
                    Field::new(format!("c{column}"), ty)
                })
                .collect::<Vec<_>>();
            let mut writer = Writer::create(path, "wide", fields).expect("new file");
            for part in 0..70_u64 {
                let columns = (0..40)
                    .map(|column| {
                        let values = (0..64_u64)
                            .map(|row| {
                                let seed = part.wrapping_mul(31).wrapping_add(row);
                                if column % 4 == 0 {
                                    Value::Varchar(format!("v{}", seed % 17))
                                } else {
                                    Value::BigInt(i64::try_from(seed % 97).expect("small"))
                                }
                            })
                            .collect::<Vec<_>>();
                        let ty = if column % 4 == 0 {
                            LogicalType::Varchar
                        } else {
                            LogicalType::BigInt
                        };
                        Vector::from_values(ty, &values).expect("a column")
                    })
                    .collect::<Vec<_>>();
                writer.append(&Chunk::new(columns).expect("forty columns")).expect("a part");
            }
            writer.finish().expect("commit");
        }

        let first = path("repeatable-one");
        let second = path("repeatable-two");
        written(&first);
        written(&second);
        let left = fs::read(&first).expect("the first file");
        let right = fs::read(&second).expect("the second file");
        assert_eq!(left.len(), right.len(), "two writes of the same rows differ in length");
        assert!(left == right, "two writes of the same rows differ in their bytes");

        // And the rows are still there, since a pair of identically wrong files would pass the
        // comparison above on its own.
        let reader = Reader::open(&first).expect("valid directory");
        assert_eq!(reader.table().rows(), 70 * 64);
        let read = reader.read(0, &[0, 1]).expect("the first part back");
        assert_eq!(read.value_at(0, 0), Value::Varchar("v0".to_owned()));
        assert_eq!(read.value_at(0, 1), Value::BigInt(0));
        fs::remove_file(first).expect("remove scratch file");
        fs::remove_file(second).expect("remove scratch file");
    }

    /// Three tables of different shapes in one file, read back by name.
    fn three_tables(path: &PathBuf) {
        let writer = Writer::create(
            path,
            "region",
            vec![
                Field::new("r_key", LogicalType::Integer),
                Field::new("r_name", LogicalType::Varchar),
            ],
        )
        .expect("new file");
        let mut writer = writer;
        writer
            .append(
                &Chunk::new(vec![
                    Vector::from_values(
                        LogicalType::Integer,
                        &[Value::Integer(0), Value::Integer(1)],
                    )
                    .expect("keys"),
                    Vector::from_values(
                        LogicalType::Varchar,
                        &[Value::Varchar("AFRICA".to_owned()), Value::Varchar("ASIA".to_owned())],
                    )
                    .expect("names"),
                ])
                .expect("two columns"),
            )
            .expect("a part");
        let mut writer = writer
            .next("empty", vec![Field::new("nothing", LogicalType::BigInt)])
            .expect("a second table");
        writer
            .append(
                &Chunk::new(vec![
                    Vector::from_values(LogicalType::BigInt, &[Value::BigInt(7)]).expect("a row"),
                ])
                .expect("one column"),
            )
            .expect("a part");
        let mut writer =
            writer.next("wide", vec![Field::new("n", LogicalType::BigInt)]).expect("a third table");
        for part in 0..70_i64 {
            let values = (0..64).map(|row| Value::BigInt(part * 64 + row)).collect::<Vec<_>>();
            writer
                .append(
                    &Chunk::new(vec![
                        Vector::from_values(LogicalType::BigInt, &values).expect("a column"),
                    ])
                    .expect("one column"),
                )
                .expect("a part");
        }
        writer.finish().expect("commit");
    }

    #[test]
    fn three_tables_in_one_file_read_back_by_name() {
        let file = path("three-tables");
        three_tables(&file);
        let catalog = Catalog::open(&file).expect("a committed catalog");
        assert_eq!(catalog.names().collect::<Vec<_>>(), ["region", "empty", "wide"]);

        let region = catalog.table("region").expect("the first table");
        assert_eq!(region.table().rows(), 2);
        assert_eq!(
            region.read(0, &[1]).expect("names").value_at(1, 0),
            Value::Varchar("ASIA".to_owned())
        );

        let wide = catalog.table("wide").expect("the third table");
        assert_eq!(wide.table().rows(), 70 * 64);
        assert_eq!(wide.read(0, &[0]).expect("the first part").value_at(0, 0), Value::BigInt(0));

        // The middle table is reached without the one after it having been touched, which is what
        // a directory per table buys over one directory of everything.
        let empty = catalog.table("empty").expect("the second table");
        assert_eq!(empty.table().rows(), 1);
        assert_eq!(empty.read(0, &[0]).expect("the row").value_at(0, 0), Value::BigInt(7));

        fs::remove_file(file).expect("remove scratch file");
    }

    #[test]
    fn a_name_the_file_does_not_hold_is_an_error_rather_than_the_first_table() {
        let file = path("three-tables-missing");
        three_tables(&file);
        let catalog = Catalog::open(&file).expect("a committed catalog");
        let error = catalog.table("nation").expect_err("no such table");
        assert!(error.message().contains("nation"), "{}", error.message());
        fs::remove_file(file).expect("remove scratch file");
    }

    #[test]
    fn a_file_of_three_tables_will_not_open_as_one() {
        let file = path("three-tables-unnamed");
        three_tables(&file);
        let error = Reader::open(&file).expect_err("more than one table");
        assert!(error.message().contains("more than one table"), "{}", error.message());
        fs::remove_file(file).expect("remove scratch file");
    }

    /// One column per storage width, because the width is what decides how many bytes a row costs.
    #[test]
    fn decimals_of_every_storage_width_round_trip() {
        let file = path("decimals");
        let widths = [(4_u8, 2_u8), (9, 2), (18, 4), (38, 6)];
        let fields = widths
            .iter()
            .enumerate()
            .map(|(index, (width, scale))| {
                Field::new(
                    format!("d{index}"),
                    LogicalType::decimal(*width, *scale).expect("a decimal type"),
                )
            })
            .collect::<Vec<_>>();
        let mut writer = Writer::create(&file, "money", fields).expect("new file");
        let rows: [i128; 3] = [-1234, 0, 999];
        let columns = widths
            .iter()
            .map(|(width, scale)| {
                let values = rows
                    .iter()
                    .map(|unscaled| Value::Decimal {
                        unscaled: *unscaled,
                        width: *width,
                        scale: *scale,
                    })
                    .collect::<Vec<_>>();
                Vector::from_values(
                    LogicalType::decimal(*width, *scale).expect("a decimal type"),
                    &values,
                )
                .expect("a decimal column")
            })
            .collect::<Vec<_>>();
        writer.append(&Chunk::new(columns).expect("four columns")).expect("a part");
        writer.finish().expect("commit");

        let reader = Reader::open(&file).expect("a committed file");
        for (index, (width, scale)) in widths.iter().enumerate() {
            assert_eq!(
                reader.table().fields()[index].ty,
                LogicalType::decimal(*width, *scale).expect("a decimal type"),
                "column {index} came back as another type"
            );
            let column = reader.read(0, &[index]).expect("the column");
            for (row, unscaled) in rows.iter().enumerate() {
                assert_eq!(
                    column.value_at(row, 0),
                    Value::Decimal { unscaled: *unscaled, width: *width, scale: *scale },
                    "column {index} row {row}"
                );
            }
        }
        fs::remove_file(file).expect("remove scratch file");
    }

    #[test]
    fn two_tables_of_one_name_are_refused_before_anything_is_committed() {
        let file = path("two-of-a-name");
        let writer = Writer::create(&file, "t", vec![Field::new("a", LogicalType::BigInt)])
            .expect("new file");
        let error = writer
            .next("t", vec![Field::new("a", LogicalType::BigInt)])
            .expect_err("the same name twice");
        assert!(error.message().contains("same name"), "{}", error.message());
        fs::remove_file(file).expect("remove scratch file");
    }

    #[test]
    fn integer_tally_counts_encoded_rows_and_declines_null_parts() {
        let file = path("integer-tally");
        let mut writer =
            Writer::create(&file, "events", vec![Field::new("source", LogicalType::SmallInt)])
                .expect("new file");
        let mut values = vec![Value::SmallInt(0); 1024];
        values[7] = Value::SmallInt(3);
        values[99] = Value::SmallInt(-2);
        values[1001] = Value::SmallInt(3);
        let column = Vector::from_values(LogicalType::SmallInt, &values).expect("integer values");
        writer.append(&Chunk::new(vec![column]).expect("one column")).expect("first part");
        values[0] = Value::Null;
        let column = Vector::from_values(LogicalType::SmallInt, &values).expect("nullable values");
        writer.append(&Chunk::new(vec![column]).expect("one column")).expect("second part");
        writer.finish().expect("commit");

        let reader = Reader::open(&file).expect("read file");
        assert_eq!(
            reader.integer_tally(0, 0).expect("valid part"),
            Some(vec![(-2, 1), (0, 1021), (3, 2)])
        );
        assert!(reader.integer_tally(1, 0).expect("valid null part").is_none());
        let catalog = Catalog::open(&file).expect("catalog");
        assert_eq!(
            catalog.integer_tally("events", 0).expect("nullable column"),
            Some(vec![(-2, 2), (0, 2041), (3, 4)])
        );
        fs::remove_file(file).expect("remove scratch file");
    }

    #[test]
    fn catalog_tallies_one_integer_column_without_opening_the_whole_table() {
        let file = path("catalog-integer-tally");
        let mut writer = Writer::create(
            &file,
            "events",
            vec![
                Field::new("noise", LogicalType::SmallInt),
                Field::new("source", LogicalType::SmallInt),
            ],
        )
        .expect("new file");
        let noise = vec![Value::SmallInt(9); 1024];
        let mut source = vec![Value::SmallInt(0); 1024];
        source[7] = Value::SmallInt(3);
        source[99] = Value::SmallInt(-2);
        let chunk = Chunk::new(vec![
            Vector::from_values(LogicalType::SmallInt, &noise).expect("noise"),
            Vector::from_values(LogicalType::SmallInt, &source).expect("source"),
        ])
        .expect("two columns");
        writer.append(&chunk).expect("append");
        writer.finish().expect("commit");

        let catalog = Catalog::open(&file).expect("catalog");
        assert_eq!(
            catalog.integer_tally("events", 1).expect("selected column"),
            Some(vec![(-2, 1), (0, 1022), (3, 1)])
        );
        assert_eq!(
            catalog.integer_tally("events", 0).expect("other column"),
            Some(vec![(9, 1024)])
        );
        fs::remove_file(file).expect("remove scratch file");
    }

    #[test]
    fn opening_the_catalog_reads_no_table_directory() {
        let file = path("catalog-only");
        three_tables(&file);
        let catalog = Catalog::open(&file).expect("a committed catalog");
        // The header and one slot, and nothing under it. The third table's directory covers seventy
        // stripes and reading it here would be the whole point of the two levels thrown away.
        assert_eq!(catalog.opening.reads, 2, "opening the catalog read more than the slot");
        assert_eq!(catalog.names().len(), 3);
        fs::remove_file(file).expect("remove scratch file");
    }

    /// The checksum answers what it has always answered, at every length its branches split on.
    ///
    /// This is a compatibility test rather than a correctness one. Nothing about the hash has to be
    /// any particular function, but a file already on disk carries the answers the version that
    /// wrote it gave, so a change here is a change that makes every stored file fail to verify. The
    /// lengths are the ones the code makes decisions about: nothing, under a block, a block exactly,
    /// a block and a word, a word and a half word, and a half word and a byte.
    ///
    /// The empty answer is the published xxHash64 vector for an empty input at seed zero, which is
    /// also a check that this is the function it says it is.
    #[test]
    fn the_checksum_answers_what_it_has_always_answered() {
        let bytes: Vec<u8> =
            (0..1000_u32).map(|at| (at.wrapping_mul(31).wrapping_add(7) % 251) as u8).collect();
        for (length, expected) in [
            (0, 0xef46_db37_51d8_e999),
            (1, 0xa96c_7f0c_e858_bbb7),
            (3, 0x56e6_9576_32a4_87f9),
            (4, 0xc60d_15b1_e3ff_8f04),
            (5, 0x8088_1585_8624_dd4e),
            (7, 0xafbe_fc3d_6c6f_9a8e),
            (8, 0x3da5_c7aa_2696_83e0),
            (9, 0x465e_c429_b13c_3892),
            (15, 0xdee8_9d8a_065a_6233),
            (16, 0x1330_489a_7767_9c80),
            (31, 0x3391_303d_485e_846e),
            (32, 0x40b7_aff7_5d45_bbc8),
            (33, 0x4997_cae4_951c_17a5),
            (39, 0x5807_28fd_5c14_5739),
            (40, 0xf95c_f6f5_c08a_3d3b),
            (63, 0x2944_b4da_fc69_b206),
            (64, 0xbb76_f6ef_19bd_5a1b),
            (65, 0x814e_0c65_4a9f_d640),
            (127, 0x00de_aab1_31cf_f89b),
            (1000, 0x9e33_00c1_cde3_c58d),
        ] {
            assert_eq!(checksum(&bytes[..length]), expected, "the checksum of {length} bytes");
        }
        assert_eq!(checksum(b"the quick brown fox jumps over the lazy dog"), 0xed71_4233_c5a9_a792);
    }
    /// A declared order survives the file, and a table that declared none stays as it was.
    ///
    /// The second half is the one worth a test. The clustering section is written only when there
    /// is a declaration, so a file of two tables where one is clustered exercises both the present
    /// and the absent branch of the decoder in one directory, which is where a length bug would
    /// show up as one table reading the other's bytes.
    #[test]
    fn a_declared_order_comes_back_out_of_the_file() {
        let path = path("clustered");
        let shipped = vec![
            Field::new("key", LogicalType::BigInt),
            Field::new("line", LogicalType::Integer),
            Field::new("shipdate", LogicalType::Date),
        ];
        let plain = vec![Field::new("a", LogicalType::Integer)];
        let stage_zero = Clustering::new(vec![2, 0, 1], Width::Month, &shipped).expect("valid");

        let mut writer = Writer::create(&path, "lineitem", shipped)
            .expect("new file")
            .declare(stage_zero.clone())
            .expect("the columns are the table's");
        let column = |ty: LogicalType, values: &[Value]| {
            Vector::from_values(ty, values).expect("the values match the type")
        };
        writer
            .append(
                &Chunk::new(vec![
                    column(
                        LogicalType::BigInt,
                        &[Value::BigInt(0), Value::BigInt(1), Value::BigInt(2), Value::BigInt(3)],
                    ),
                    column(
                        LogicalType::Integer,
                        &[
                            Value::Integer(1),
                            Value::Integer(1),
                            Value::Integer(1),
                            Value::Integer(1),
                        ],
                    ),
                    column(
                        LogicalType::Date,
                        &[Value::Date(0), Value::Date(1), Value::Date(2), Value::Date(3)],
                    ),
                ])
                .expect("three columns"),
            )
            .expect("four rows");
        let mut writer = writer.next("nation", plain).expect("a second table");
        writer
            .append(
                &Chunk::new(vec![column(LogicalType::Integer, &[Value::Integer(7)])])
                    .expect("one column"),
            )
            .expect("one row");
        writer.finish().expect("commit");

        let catalog = Catalog::open(&path).expect("reopen");
        let lineitem = catalog.table("lineitem").expect("the clustered table");
        assert_eq!(lineitem.table().clustering(), Some(&stage_zero));
        let nation = catalog.table("nation").expect("the plain table");
        assert_eq!(nation.table().clustering(), None, "nobody declared one here");

        // And the rows are still the rows, because the section goes on the end of the directory
        // and the easy way to break that is to leave the cursor somewhere the next read trusts.
        assert_eq!(lineitem.table().rows(), 4);
        assert_eq!(nation.table().rows(), 1);
        fs::remove_file(&path).ok();
    }

    /// A declaration naming a column the table does not have is refused where it is made.
    #[test]
    fn a_declaration_off_the_end_of_the_table_never_reaches_the_file() {
        let path = path("clustered-bad");
        let writer = Writer::create(&path, "items", vec![Field::new("a", LogicalType::Integer)])
            .expect("new file");
        let four =
            (0..4).map(|at| Field::new(format!("c{at}"), LogicalType::Integer)).collect::<Vec<_>>();
        let wrong = Clustering::new(vec![3], Width::Exact, &four).expect("valid against four");
        assert!(writer.declare(wrong).is_err(), "the table has one column, not four");
        fs::remove_file(&path).ok();
    }

    /// The sorted order is the byte order, whatever the values do before they differ.
    ///
    /// A block handed out of the writer's lock to be encoded, and given back in whatever order the
    /// stripes happen to finish in, is the same block with the same signature as one encoded in
    /// place, and lands in the same position.
    #[test]
    fn blocks_handed_out_and_given_back_out_of_order_are_the_blocks_encoded_in_place() {
        let values = (0..PAYLOAD_SAMPLE_BLOCKS * TEXT_PAYLOAD_VALUES * 2 + 100)
            .map(|at| format!("http://example{}.test/page/{at:06}", at % 7))
            .collect::<Vec<_>>();
        let filled = || {
            let mut dictionary = GlobalDictionary::new();
            for value in &values {
                dictionary.code(value).expect("a code for every value");
            }
            dictionary.settle().expect("a shape");
            dictionary
        };
        let mut in_place = filled();
        in_place.finish_blocks().expect("every block encodes");

        let mut handed = filled();
        let out = handed.hand_out(3);
        assert_eq!(out.len(), PAYLOAD_SAMPLE_BLOCKS * 2, "every sealed block goes out");
        assert!(handed.waiting.is_empty(), "and none is left to be encoded under the lock");
        for job in out.iter().rev() {
            assert_eq!(job.place().0, 3, "a block goes back to the column it came from");
            handed.take_back(job.place().1, job.encode().expect("encodes")).expect("taken back");
        }
        assert!(handed.early.is_empty(), "nothing is waiting on a gap");
        handed.finish_blocks().expect("the last block encodes");

        assert_eq!(handed.blocks, in_place.blocks, "the same blocks in the same order");
        assert_eq!(handed.grams, in_place.grams, "with the same signatures");
    }

    /// A block given back twice is a bug in whoever gave it, and is said rather than written twice.
    #[test]
    fn a_block_given_back_twice_is_refused() {
        let mut dictionary = GlobalDictionary::new();
        for at in 0..PAYLOAD_SAMPLE_BLOCKS * TEXT_PAYLOAD_VALUES {
            dictionary.code(&format!("value {at}")).expect("a code");
        }
        dictionary.settle().expect("a shape");
        let out = dictionary.hand_out(0);
        let last = out.last().expect("blocks went out");
        let at = last.place().1;
        dictionary.take_back(at, last.encode().expect("encodes")).expect("taken back once");
        assert!(dictionary.take_back(at, last.encode().expect("encodes")).is_err());
    }

    /// The values here are the shape the sort is built for and the shape a comparison sort is worst
    /// at: a common scheme, a handful of hosts, and a path that only decides the pair thirty bytes
    /// in. They also cover what the bucketing has to get right at the edges, which is a value that
    /// has run out where another carries on, the empty value, and enough entries to take the range
    /// down through several passes and out the bottom into the comparison that finishes it.
    #[test]
    fn the_dictionary_order_is_the_byte_order_however_deep_the_values_agree() {
        let mut values = vec![String::new(), "http://".to_owned()];
        for host in 0..7 {
            for path in 0..30 {
                values.push(format!("http://example{host}.test/page/{path:04}/index.html"));
                values.push(format!("http://example{host}.test/page/{path:04}"));
            }
        }
        values.push("http://example0.test/page/0000/index.htmlx".to_owned());

        let mut dictionary = GlobalDictionary::new();
        for value in &values {
            dictionary.code(value).expect("a code for every value");
        }
        dictionary.finish_blocks().expect("the last block encodes");
        let ranked = dictionary.ranked(None).expect("a sorted order");
        assert_eq!(ranked.len(), values.len(), "one entry a distinct value");

        let spellings = dictionary_values(&dictionary);
        let seen = ranked
            .iter()
            .map(|&(_, code)| {
                String::from_utf8(spellings[code as usize].clone()).expect("text in, text out")
            })
            .collect::<Vec<_>>();
        let mut wanted = values.clone();
        wanted.sort_unstable();
        assert_eq!(seen, wanted, "the order is the order the bytes give");

        for &(carried, code) in &ranked {
            let value = &spellings[code as usize];
            assert_eq!(carried, head(value), "the head belongs to the value it is filed with");
        }
    }

    /// Picking the commonest entries leaves exactly what sorting all of them and cutting left.
    ///
    /// The counts here are deliberately full of ties, including a tie that straddles the cut, which
    /// is where a partition and a sort can disagree if the comparison they are given is not total.
    #[test]
    fn the_commonest_entries_are_the_ones_a_full_sort_would_have_kept() {
        let entry =
            |value: u32, count: u64| FrequencyEntry { value: FrequencyValue::Code(value), count };
        let mut all = (0..FREQUENCY_ENTRIES as u32 * 3)
            .map(|code| entry(code, u64::from(code % 7) + 1))
            .collect::<Vec<_>>();
        all.push(FrequencyEntry { value: FrequencyValue::Null, count: 4 });

        let mut sorted = all.clone();
        sorted.sort_unstable_by(|left, right| {
            right.count.cmp(&left.count).then_with(|| frequency_order(left.value, right.value))
        });
        let wanted_omitted = sorted[FREQUENCY_ENTRIES].count;
        sorted.truncate(FREQUENCY_ENTRIES);

        let mut picked = all.clone();
        let omitted = keep_most_frequent(&mut picked);
        assert_eq!(omitted, wanted_omitted, "the largest count that did not make the cut");
        assert_eq!(picked.len(), FREQUENCY_ENTRIES, "the cut is where it says it is");
        assert!(
            picked
                .iter()
                .zip(&sorted)
                .all(|(one, two)| one.value == two.value && one.count == two.count),
            "the same entries in the same order"
        );

        let mut short = all[..FREQUENCY_ENTRIES - 1].to_vec();
        let omitted = keep_most_frequent(&mut short);
        assert_eq!(omitted, 0, "nothing is omitted when everything fits");
        assert!(short.windows(2).all(|pair| pair[0].count >= pair[1].count), "still in order");
    }

    /// A dictionary too small to bucket, and one with nothing in it, come back in order too.
    #[test]
    fn a_short_dictionary_sorts_without_a_bucketing_pass() {
        let empty = GlobalDictionary::new();
        assert!(empty.ranked(None).expect("an empty order").is_empty(), "nothing in, nothing out");

        let mut dictionary = GlobalDictionary::new();
        for value in ["pear", "apple", "", "apples", "app"] {
            dictionary.code(value).expect("a code for every value");
        }
        dictionary.finish_blocks().expect("the one block encodes");
        let spellings = dictionary_values(&dictionary);
        let seen = dictionary
            .ranked(None)
            .expect("a sorted order")
            .iter()
            .map(|&(_, code)| spellings[code as usize].clone())
            .collect::<Vec<_>>();
        let wanted: Vec<Vec<u8>> =
            [&b""[..], b"app", b"apple", b"apples", b"pear"].iter().map(|v| v.to_vec()).collect();
        assert_eq!(seen, wanted, "shorter first where one runs out inside another");
    }

    /// A demoted dictionary gives back what it kept for looking values up, the load profile is told,
    /// and it refuses any value after that.
    #[test]
    fn a_demoted_dictionary_holds_less_and_takes_no_more_values() {
        let profile = LoadProfile::begin("demoted");
        let mut dictionary = GlobalDictionary::new();
        for value in 0..50_000 {
            dictionary.code(&format!("https://example.com/page/{value}")).expect("a code");
        }
        let (_, grown) = dictionary.recharge(Some(&profile));
        assert_eq!(profile.held(), grown, "the profile holds what the dictionary does");

        dictionary.demote();
        let (before, after) = dictionary.recharge(Some(&profile));
        assert_eq!(before, grown);
        // What stays is the ends, the counts and the blocks not yet written, which a load writes
        // as it goes, so here the drop is the hash tables and the check hashes.
        assert!(after < grown - grown / 4, "the lookup is let go of: {after} of {grown}");
        assert_eq!(profile.held(), after, "the profile was told about the drop");
        assert!(dictionary.code("one more").is_err(), "a demoted dictionary takes no values");

        dictionary.demote();
        assert_eq!(
            dictionary.recharge(Some(&profile)),
            (after, after),
            "demoting twice is a no-op"
        );
        assert_eq!(dictionary.values(), 50_000, "the values coded before stay");
    }
}
