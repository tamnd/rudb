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

use std::cmp::Ordering;
use std::collections::{HashMap, VecDeque};
use std::fs::{File, OpenOptions};
use std::io::{Read, Seek, SeekFrom};
use std::mem::{size_of, size_of_val};
use std::path::Path;
use std::slice;
use std::sync::atomic::{AtomicUsize, Ordering as Atomic};
use std::sync::{Arc, Mutex, OnceLock};

use rudb_common::bounds::{self, Bound, Op, scaled_as};
use rudb_common::{Clustering, Error, Field, LogicalType, PhysicalType, Result, Value, Width};
use rudb_encoding::{bitpack, chooser, integer, string};
use rudb_storage::sieve::Sieve;
use rudb_storage::{Probe, Range, Zone};
use rudb_vector::string::StringColumn;
use rudb_vector::validity::Validity;
use rudb_vector::{Buffer, Chunk, Data, Packed, TextSource, Vector, search_below};

pub mod graph;
pub mod section;
pub mod stats;
mod zones;

pub use section::Section;
pub use zones::{Common, Stripes, distincts};

const MAGIC: &[u8; 8] = b"RUDBNV10";
const DIRECTORY: &[u8; 8] = b"RUDBDI10";
const CATALOG: &[u8; 8] = b"RUDBCA10";
const FORMAT: u32 = 25;

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
/// catalog that ends where the tables end reads as a catalog with no views in it.
///
/// This is not a general compatibility promise. Four formats are readable because there was a
/// specific reason for each, and the list shrinks again the moment the older ones stop being worth
/// carrying.
const READABLE: &[u32] = &[22, 23, 24, FORMAT];

const HEADER: u64 = 80;
const SLOT_BYTES: usize = 28;
const MAX_PAGE: usize = 256 * 1024 * 1024;
const MAX_DIRECTORY: usize = 128 * 1024 * 1024;
const FREQUENCIES: &[u8; 8] = b"RUDBFQ2\0";
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
/// The graph section table, written after the clustering declaration and written even when empty.
///
/// Same convention and the same reason as the block above it, with one difference: this one is
/// always there, so a file written by this build says which sections it has rather than leaving a
/// reader to infer it from where the bytes ran out. Section 3.1 of the graph spec is what makes
/// that safe to add without a format bump, because a table with no sections answers every query
/// the way it did before, only without the graph path.
const SECTIONS: &[u8; 8] = b"RUDBSE1\0";

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
const FREQUENCY_ORDINALS: usize = 65_536;
/// The most threads the two per column passes at the end of a commit are spread over.
///
/// A table like `hits` has ninety numeric columns, so on a machine with more cores than this the
/// cap is what decides how long the frequencies take rather than the columns are. It is here at all
/// because each worker holds a candidate table and a decoded part, and a hundred of those at once
/// on a narrow machine would be worse than waiting.
const MAX_FREQUENCY_WORKERS: usize = 32;

/// The most threads one stripe's encode is spread over.
///
/// Higher than the frequency cap because this is the load itself rather than a pass at the end of
/// it, and the work is one column of sixty four parts, which is large enough that a thread that
/// takes one is not a thread that was started for nothing. A machine with more cores than this has
/// the rest of them on the Parquet read, which is still one thread and is the other half of #808.
const MAX_ENCODE_WORKERS: usize = 32;

/// The most bytes one column of one part may spend on a membership sieve.
///
/// A part is a thousand rows, so a filter sized for every one of them being distinct is about
/// thirteen hundred bytes and this never binds in practice. It is here so that a part that somehow
/// arrives much wider than a vector cannot put an unbounded index in the file. What does bind is the
/// rule in `encode_column` that a sieve may not be as large as the part it indexes, which is a cap
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
    const P1: u64 = 11_400_714_785_074_694_791;
    const P2: u64 = 14_029_467_366_897_019_727;
    const P3: u64 = 1_609_587_929_392_839_161;
    const P4: u64 = 9_650_029_242_287_828_579;
    const P5: u64 = 2_870_177_450_012_600_261;
    let round = |state: u64, word: u64| {
        state.wrapping_add(word.wrapping_mul(P2)).rotate_left(31).wrapping_mul(P1)
    };
    let merge = |state: u64, lane: u64| (state ^ round(0, lane)).wrapping_mul(P1).wrapping_add(P4);
    let word = |chunk: &[u8]| u64::from_le_bytes(chunk.try_into().expect("eight checksum bytes"));

    // Asked for before the loop rather than after it, because a `ChunksExact` settles what it
    // cannot divide when it is built and hands back the same tail whether it has been walked or not.
    let mut blocks = bytes.chunks_exact(32);
    let mut rest = blocks.remainder();
    let mut hash = if bytes.len() >= 32 {
        let mut one = P1.wrapping_add(P2);
        let mut two = P2;
        let mut three = 0;
        let mut four = 0_u64.wrapping_sub(P1);
        for block in blocks.by_ref() {
            one = round(one, word(&block[..8]));
            two = round(two, word(&block[8..16]));
            three = round(three, word(&block[16..24]));
            four = round(four, word(&block[24..]));
        }
        let combined = one
            .rotate_left(1)
            .wrapping_add(two.rotate_left(7))
            .wrapping_add(three.rotate_left(12))
            .wrapping_add(four.rotate_left(18));
        merge(merge(merge(merge(combined, one), two), three), four)
    } else {
        P5
    };
    hash = hash.wrapping_add(bytes.len() as u64);
    let mut words = rest.chunks_exact(8);
    for chunk in words.by_ref() {
        hash ^= round(0, word(chunk));
        hash = hash.rotate_left(27).wrapping_mul(P1).wrapping_add(P4);
    }
    rest = words.remainder();
    if rest.len() >= 4 {
        let (head, tail) = rest.split_at(4);
        let quarter = u32::from_le_bytes(head.try_into().expect("four checksum bytes"));
        hash ^= u64::from(quarter).wrapping_mul(P1);
        hash = hash.rotate_left(23).wrapping_mul(P2).wrapping_add(P3);
        rest = tail;
    }
    for &byte in rest {
        hash ^= u64::from(byte).wrapping_mul(P5);
        hash = hash.rotate_left(11).wrapping_mul(P1);
    }
    hash ^= hash >> 33;
    hash = hash.wrapping_mul(P2);
    hash ^= hash >> 29;
    hash = hash.wrapping_mul(P3);
    hash ^ (hash >> 32)
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
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FrequencyOccurrences {
    /// Upper bound for the frequency of every value absent from the fetched rows.
    pub omitted_max: u64,
    /// Table-wide row ordinals in ascending order.
    pub ordinals: Vec<u64>,
}

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
    memberships: Vec<Option<Page>>,
    /// One page per column holding the membership sieve of every part of the stripe, for the
    /// columns that have one. A column whose parts all declined a sieve has no page at all.
    sieves: Vec<Option<Page>>,
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
    part_ranges: Vec<Option<Page>>,
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
    frequencies: Vec<Option<FrequencySummary>>,
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
}

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

/// Appends pages and commits a new directory for one table.
#[derive(Debug)]
struct GlobalDictionary {
    primary: HashMap<u64, u32>,
    collisions: HashMap<u64, Vec<u32>>,
    offsets: Vec<u32>,
    payload: Vec<u8>,
    counts: Vec<u64>,
    nulls: u64,
}

impl GlobalDictionary {
    fn new() -> Self {
        Self {
            primary: HashMap::new(),
            collisions: HashMap::new(),
            offsets: vec![0],
            payload: Vec::new(),
            counts: Vec::new(),
            nulls: 0,
        }
    }

    fn bytes(&self, code: u32) -> Option<&[u8]> {
        let start = *self.offsets.get(code as usize)? as usize;
        let end = *self.offsets.get(code as usize + 1)? as usize;
        self.payload.get(start..end)
    }

    fn code(&mut self, text: &str) -> Result<u32> {
        let hash = checksum(text.as_bytes());
        if let Some(&code) = self.primary.get(&hash) {
            if self.bytes(code) == Some(text.as_bytes()) {
                return Ok(code);
            }
            if let Some(codes) = self.collisions.get(&hash) {
                if let Some(code) =
                    codes.iter().copied().find(|&code| self.bytes(code) == Some(text.as_bytes()))
                {
                    return Ok(code);
                }
            }
            let code = self.insert(text)?;
            self.collisions.entry(hash).or_default().push(code);
            return Ok(code);
        }
        let code = self.insert(text)?;
        self.primary.insert(hash, code);
        Ok(code)
    }

    fn insert(&mut self, text: &str) -> Result<u32> {
        let code = u32::try_from(self.offsets.len() - 1)
            .map_err(|_| invalid("global dictionary has too many values"))?;
        self.payload.extend_from_slice(text.as_bytes());
        self.offsets.push(
            u32::try_from(self.payload.len())
                .map_err(|_| invalid("global dictionary payload exceeds 4 GiB"))?,
        );
        self.counts.push(0);
        Ok(code)
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
    fn ranked(&self) -> Vec<(u64, u32)> {
        let count = self.offsets.len() - 1;
        let mut codes = (0..count as u32).collect::<Vec<_>>();
        sort_by_value(&mut codes, |code| self.bytes(code).unwrap_or_default());
        codes.into_iter().map(|code| (head(self.bytes(code).unwrap_or_default()), code)).collect()
    }

    fn observe(&mut self, code: u32, null: bool) -> Result<()> {
        if null {
            self.nulls = self.nulls.saturating_add(1);
            return Ok(());
        }
        let count = self
            .counts
            .get_mut(code as usize)
            .ok_or_else(|| invalid("global dictionary count code is out of range"))?;
        *count = count.saturating_add(1);
        Ok(())
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
    file: File,
    /// Where the next write goes, counted here rather than asked of the file.
    ///
    /// The file's own cursor is not ours. Building the numeric frequencies reads pages back through
    /// [`read_at`], and a positional read is only positional about where it reads from: `pread`
    /// leaves the cursor alone, and the call Windows has for it moves the cursor to the end of what
    /// it read. A writer that asked the file where it was would then write the directory over a
    /// page it had already written, which is what it did.
    at: u64,
    table: Table,
    generation: u64,
    /// The first and the last source position in every stripe, in the order the stripes were
    /// written.
    order: Vec<((u64, u64), (u64, u64))>,
    next_order: u64,
    dictionaries: Vec<Option<GlobalDictionary>>,
    /// One per column, folding the rows into a summary and a sketch as they go past.
    ///
    /// `None` for a column with no hash rule, which is the interval and the nested types. See
    /// [`stats::Gather`] for why the statistics are built here rather than by reading the file back
    /// once it is committed.
    gathers: Vec<Option<stats::Gather>>,
    pending: Vec<PendingChunk>,
    /// The tables already closed in this generation, in the order they were written.
    closed: Vec<Entry>,
    /// The views the next commit writes down, which [`Writer::with_views`] sets.
    ///
    /// Carried forward from the committed generation by [`Writer::open`], so a writer that was only
    /// opened to append a table does not have to know about views to avoid dropping them.
    views: Vec<ViewEntry>,
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

/// One column's share of a stripe, which is what one encode worker produces.
///
/// Indexed by part, so a stripe is a column of these and the write loop reads down one of them.
/// That is also the order the loop wanted: `flush_pending` walks a column at a time and lays its
/// parts next to each other, and it used to reach across a row of parts to do it.
#[derive(Debug)]
struct ColumnStripe {
    pages: Vec<Vec<u8>>,
    codes: Vec<Option<Vec<u32>>>,
    sieves: Vec<Option<Sieve>>,
    ranges: Vec<Range>,
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
/// See [`Writer::encode_column`]. A stripe is up to [`STRIPE_PARTS`] parts, so most tables give it
/// far more than this and it binds only on a table that is smaller than one stripe. A handful of
/// rows says nothing about whether a column repeats itself, and the answer that costs nothing when
/// the sample is that small is the one the writer has always given, which is to keep the dictionary.
const DICTIONARY_DECIDE_ROWS: usize = 4_096;

/// Out of ten. A varchar column loses its dictionary when more than this many rows in ten of the
/// first stripe held a value that stripe had not seen before.
///
/// See [`Writer::encode_column`]. Nine and not five, because the properties a dictionary buys are
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
        for field in &fields {
            type_tag(&field.ty)?;
        }
        let name = name.into();
        let path = path.as_ref();
        let (_, size, slot, bytes, _) = slot_bytes(path)?;
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
        let file = OpenOptions::new().write(true).read(true).open(path).map_err(io)?;
        Ok(Self {
            file,
            // The end of the file, so that the committed generation's catalog stays where its slot
            // says it is and keeps naming a file a reader can still open.
            at: size,
            dictionaries: fields
                .iter()
                .map(|field| (field.ty == LogicalType::Varchar).then(GlobalDictionary::new))
                .collect(),
            gathers: fields.iter().map(|field| stats::Gather::new(&field.ty, generation)).collect(),
            table: Table {
                name,
                dictionaries: vec![None; fields.len()],
                distincts: vec![None; fields.len()],
                fields,
                stripes: Vec::new(),
                rows: 0,
                frequencies: Vec::new(),
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
        for field in &fields {
            type_tag(&field.ty)?;
        }
        let file =
            OpenOptions::new().write(true).read(true).create_new(true).open(path).map_err(io)?;
        let mut header = [0; HEADER as usize];
        header[..8].copy_from_slice(MAGIC);
        header[8..12].copy_from_slice(&FORMAT.to_le_bytes());
        write_at(&file, 0, &header)?;
        Ok(Self {
            file,
            at: HEADER,
            dictionaries: fields
                .iter()
                .map(|field| (field.ty == LogicalType::Varchar).then(GlobalDictionary::new))
                .collect(),
            gathers: fields.iter().map(|field| stats::Gather::new(&field.ty, 1)).collect(),
            table: Table {
                name: name.into(),
                dictionaries: vec![None; fields.len()],
                distincts: vec![None; fields.len()],
                fields,
                stripes: Vec::new(),
                rows: 0,
                frequencies: Vec::new(),
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
        let file =
            OpenOptions::new().write(true).read(true).create_new(true).open(path).map_err(io)?;
        let mut header = [0; HEADER as usize];
        header[..8].copy_from_slice(MAGIC);
        header[8..12].copy_from_slice(&FORMAT.to_le_bytes());
        write_at(&file, 0, &header)?;
        let catalog = encode_catalog(&[], views)?;
        write_at(&file, HEADER, &catalog)?;
        // The same two syncs in the same order as [`Writer::finish`], and for the same reason. The
        // catalog is on the disk before the slot names it, so a file this is interrupted in the
        // middle of is a header with no valid slot rather than a slot pointing at nothing.
        file.sync_all().map_err(io)?;
        let slot = Slot {
            offset: HEADER,
            length: u32::try_from(catalog.len()).map_err(|_| invalid("catalog length overflow"))?,
            generation: 1,
            hash: checksum(&catalog),
        };
        write_at(&file, slot_offset(1), &slot.bytes())?;
        file.sync_all().map_err(io)?;
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
            at,
            generation,
            closed,
            views,
            dictionaries: fields
                .iter()
                .map(|field| (field.ty == LogicalType::Varchar).then(GlobalDictionary::new))
                .collect(),
            gathers: fields.iter().map(|field| stats::Gather::new(&field.ty, generation)).collect(),
            table: Table {
                name,
                dictionaries: vec![None; fields.len()],
                distincts: vec![None; fields.len()],
                fields,
                stripes: Vec::new(),
                rows: 0,
                frequencies: Vec::new(),
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
        write_at(&self.file, self.at, bytes)?;
        self.at = self
            .at
            .checked_add(bytes.len() as u64)
            .ok_or_else(|| invalid("native file length overflow"))?;
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

    /// Encodes one column's parts of a stripe, and on the first stripe decides whether the column
    /// should have a dictionary at all.
    ///
    /// Every varchar column starts with one, because the writer cannot know what is in a column
    /// before it has seen some of it. A global dictionary is the right shape for a column of a few
    /// dozen values repeated down the table: the pages become small integers, a filter against a
    /// literal is one search of the sorted order rather than a comparison a row, and a group by is
    /// on the codes. It is the wrong shape for a column whose values are nearly all different.
    /// There the codes are as wide as row numbers, nothing is saved on the pages, and the
    /// membership index of a stripe is a list of very nearly every code in the column. On TPC-H the
    /// orders table written on its own goes from 52.3 MB to 41.4 MB, the load from 6.9 s to 5.8 s,
    /// and `select o_comment from orders` from 1.810 G instructions to 1.213 G, which is what the
    /// rudb parquet reader takes over the same values.
    ///
    /// So the first stripe of a column is the sample and the decision is made once on it. Once,
    /// rather than per stripe, because the codes of one column have to mean the same thing in every
    /// page of it, and a column that changed its mind halfway would need its earlier stripes
    /// rewritten. The first stripe is re-encoded when the answer comes out against the dictionary,
    /// which is the one stripe that pays for the decision.
    ///
    /// The threshold is deliberately near the top. [`DICTIONARY_DISTINCT_IN_TEN`] of the sample has
    /// to be values never seen before, which is a column with essentially no repeats. Everything
    /// with real repetition keeps its dictionary and keeps every property that hangs off it, and
    /// nothing is claimed here about where between the two the crossover really sits.
    ///
    /// Nothing here is shared with another column. The dictionary belongs to this one, the sieve
    /// reads only this one, and the page bytes go in a vector of this one's own. That is why the
    /// fan out below can hand a whole column to a thread and take a plain `&mut` on the dictionary
    /// rather than making it something several threads can grow at once, which is the harder half
    /// of #808 and is still open.
    fn encode_column(
        index: usize,
        held: &[PendingChunk],
        dictionary: &mut Option<GlobalDictionary>,
    ) -> Result<ColumnStripe> {
        // Empty means nothing has been written through it yet, so this is the column's first stripe
        // and the only stripe the decision below is allowed to be made on.
        let deciding = dictionary.as_ref().is_some_and(|held| held.offsets.len() == 1);
        let stripe = Self::encode_pages(index, held, dictionary.as_mut())?;
        if !deciding {
            return Ok(stripe);
        }
        let rows: usize = held.iter().map(|pending| pending.chunk.len()).sum();
        let distinct = dictionary.as_ref().map_or(0, |held| held.offsets.len() - 1);
        if rows < DICTIONARY_DECIDE_ROWS
            || distinct.saturating_mul(10) <= rows.saturating_mul(DICTIONARY_DISTINCT_IN_TEN)
        {
            return Ok(stripe);
        }
        *dictionary = None;
        Self::encode_pages(index, held, None)
    }

    /// One column's parts of a stripe, with whatever dictionary it was given.
    fn encode_pages(
        index: usize,
        held: &[PendingChunk],
        mut dictionary: Option<&mut GlobalDictionary>,
    ) -> Result<ColumnStripe> {
        let mut stripe = ColumnStripe {
            pages: Vec::with_capacity(held.len()),
            codes: Vec::with_capacity(held.len()),
            sieves: Vec::with_capacity(held.len()),
            ranges: Vec::with_capacity(held.len()),
        };
        for pending in held {
            let column = pending.chunk.column(index)?;
            let (bytes, unique) = encode(column, dictionary.as_deref_mut())?;
            if bytes.len() > MAX_PAGE {
                return Err(invalid("column page exceeds the configured bound"));
            }
            // The range is built first because the sieve reads it rather than walking the column a
            // second time to find out how wide it is.
            let range = Range::of(column);
            // A column with a global dictionary already has an exact membership index per stripe,
            // so an approximate one beside it would cost a hash of every string in the table to
            // answer a question that is already answered. What it would buy is the finer grain, a
            // part rather than a stripe, and that is worth coming back for on its own.
            //
            // A sieve at least as large as the part it indexes is not written. A reader reads the
            // sieve to decide whether to read the part, so when the sieve is the larger of the two
            // it has already spent more than the read it is trying to avoid, and that holds even if
            // it rejects every time. It is a necessary condition rather than the whole rule, which
            // is that a sieve pays when its bytes are under the rejection rate times the part's,
            // but the rejection rate depends on what a query probes for and the writer does not
            // know that. The necessary half needs two numbers that are both in hand here.
            let sieve = match dictionary {
                Some(_) => None,
                None => Sieve::of(column, &range, SIEVE_BUDGET)
                    .filter(|sieve| sieve.len() < bytes.len()),
            };
            stripe.pages.push(bytes);
            stripe.codes.push(unique);
            stripe.sieves.push(sieve);
            stripe.ranges.push(range);
        }
        Ok(stripe)
    }

    /// Encodes a whole stripe, one column to a worker.
    ///
    /// The columns are handed out through a queue rather than dealt in equal piles, because they
    /// are nothing like equal: `URL` on ClickBench is a global dictionary of sixty one million
    /// strings and `IsMobile` is a byte. A pile that happened to hold the four large string columns
    /// would be the whole stripe and the other workers would be waiting on it. The queue is sorted
    /// so the expensive ones are taken first, which is the classic answer to a last job that runs
    /// longer than everything after it.
    /// One column of one stripe: the pages it encodes to, and the statistics it folds into.
    ///
    /// The two together rather than in two passes, because the stripe's rows are in memory once and
    /// this is the moment they are. Reading them back afterwards is what `stats::build_summary`
    /// does and what [`stats::Gather`] exists to avoid.
    ///
    /// A column whose vector the chunk cannot produce is skipped rather than refused, because
    /// `encode_column` below is about to fail on the same chunk and its message is the better one.
    fn encode_one(
        index: usize,
        held: &[PendingChunk],
        dictionary: &mut Option<GlobalDictionary>,
        gather: &mut Option<stats::Gather>,
    ) -> Result<ColumnStripe> {
        if let Some(gather) = gather {
            gather.stripe(held.iter().filter_map(|pending| pending.chunk.column(index).ok()));
        }
        Self::encode_column(index, held, dictionary)
    }

    fn encode_columns(&mut self, held: &[PendingChunk]) -> Result<Vec<ColumnStripe>> {
        let width = self.table.fields.len();
        let workers = std::thread::available_parallelism()
            .map_or(1, usize::from)
            .min(MAX_ENCODE_WORKERS)
            .min(width);
        if workers <= 1 || held.len() <= 1 {
            return self
                .dictionaries
                .iter_mut()
                .zip(self.gathers.iter_mut())
                .enumerate()
                .map(|(index, (dictionary, gather))| {
                    Self::encode_one(index, held, dictionary, gather)
                })
                .collect();
        }
        // The dictionaries are moved out and back rather than borrowed, because a worker that takes
        // the next column off a queue cannot be holding a borrow of the vector the queue came from.
        // The gathers ride along with them for the same reason and so that one column's statistics
        // are folded on the thread that is already walking that column.
        let mut jobs: Vec<(usize, Option<GlobalDictionary>, Option<stats::Gather>)> =
            std::mem::take(&mut self.dictionaries)
                .into_iter()
                .zip(std::mem::take(&mut self.gathers))
                .enumerate()
                .map(|(index, (dictionary, gather))| (index, dictionary, gather))
                .collect();
        // Popped from the back, so the expensive columns go last in the vector.
        jobs.sort_by_key(|(index, _, _)| weight(&self.table.fields[*index].ty));
        let queue = Mutex::new(jobs);
        let pieces = std::thread::scope(|scope| {
            (0..workers)
                .map(|_| {
                    scope.spawn(|| {
                        let mut mine = Vec::new();
                        loop {
                            let taken = queue
                                .lock()
                                .map_err(|_| Error::internal("a native encode worker panicked"))?
                                .pop();
                            let Some((index, mut dictionary, mut gather)) = taken else { break };
                            let encoded =
                                Self::encode_one(index, held, &mut dictionary, &mut gather)?;
                            mine.push((index, dictionary, gather, encoded));
                        }
                        Ok(mine)
                    })
                })
                .collect::<Vec<_>>()
                .into_iter()
                .map(|handle| {
                    handle.join().map_err(|_| Error::internal("a native encode worker panicked"))?
                })
                .collect::<Result<Vec<_>>>()
        })?;
        let mut dictionaries: Vec<Option<GlobalDictionary>> = (0..width).map(|_| None).collect();
        let mut gathers: Vec<Option<stats::Gather>> = (0..width).map(|_| None).collect();
        let mut encoded: Vec<Option<ColumnStripe>> = (0..width).map(|_| None).collect();
        for piece in pieces {
            for (index, dictionary, gather, stripe) in piece {
                dictionaries[index] = dictionary;
                gathers[index] = gather;
                encoded[index] = Some(stripe);
            }
        }
        self.dictionaries = dictionaries;
        self.gathers = gathers;
        encoded
            .into_iter()
            .map(|stripe| stripe.ok_or_else(|| Error::internal("a column was never encoded")))
            .collect()
    }

    /// Writes the buffered parts as one stripe, each column's parts contiguous on disk.
    fn flush_pending(&mut self) -> Result<()> {
        if self.pending.is_empty() {
            return Ok(());
        }
        let width = self.table.fields.len();
        // Held here rather than read off the writer, because writing a page needs the writer and
        // the borrow checker is right that those are two different uses of it.
        let mut held = std::mem::take(&mut self.pending);
        let parts = held.len();
        let encoded = self.encode_columns(&held)?;
        let mut pages = Vec::with_capacity(width);
        let mut memberships = vec![None; width];
        let mut ranges = Vec::with_capacity(width);
        let mut index = Vec::with_capacity(width.saturating_mul(index_section(parts)?));
        for stripe in &encoded {
            let offset = self.at;
            let section = index.len();
            let mut length = 0_usize;
            for bytes in &stripe.pages {
                write_at(&self.file, self.at + length as u64, bytes)?;
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
        for pending in held.drain(..) {
            let part = pending.chunk.len();
            rows = rows.checked_add(part).ok_or_else(|| invalid("row count overflow"))?;
            lengths.push(u32::try_from(part).map_err(|_| invalid("part row count overflow"))?);
            span = Some(
                span.map_or((pending.order, pending.order), |(first, _)| (first, pending.order)),
            );
        }
        self.order.push(span.ok_or_else(|| invalid("a stripe was flushed with no parts"))?);
        self.table.stripes.push(Stripe {
            rows,
            parts: lengths,
            index,
            pages,
            memberships,
            sieves,
            part_ranges,
            zone: Zone::from_ranges(ranges),
        });
        // Back where it came from, empty, so the next stripe buffers into the same allocation.
        self.pending = held;
        Ok(())
    }

    /// Finds exact heavy hitters without keeping a hash table for every numeric column while the
    /// load is live. The pages are already in the target file, so one column at a time uses a
    /// bounded Misra-Gries candidate table and then recounts only those candidates.
    fn numeric_frequency(&self, column: usize) -> Result<Option<FrequencySummary>> {
        let ty = &self.table.fields[column].ty;
        if !matches!(
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
                | LogicalType::Timestamp
        ) {
            return Ok(None);
        }
        let mut candidates: HashMap<FrequencyValue, u32> = HashMap::new();
        let mut decrements = 0_u64;
        self.visit_numeric(column, |_, value| {
            if let Some(count) = candidates.get_mut(&value) {
                *count = count.saturating_add(1);
            } else if candidates.len() < FREQUENCY_CANDIDATES {
                candidates.insert(value, 1);
            } else {
                candidates.retain(|_, count| {
                    *count -= 1;
                    *count != 0
                });
                decrements = decrements.saturating_add(1);
            }
        })?;
        let (exact, ordinals) = if decrements == 0 {
            (
                candidates
                    .into_iter()
                    .map(|(value, count)| (value, u64::from(count)))
                    .collect::<HashMap<_, _>>(),
                Vec::new(),
            )
        } else {
            let mut lower = candidates.values().copied().collect::<Vec<_>>();
            lower.sort_unstable_by(|left, right| right.cmp(left));
            if lower.len() < FREQUENCY_BUILD_RANK
                || u64::from(lower[FREQUENCY_BUILD_RANK - 1]) <= decrements
            {
                return Ok(None);
            }
            let mut exact =
                candidates.into_keys().map(|value| (value, 0_u64)).collect::<HashMap<_, _>>();
            let mut ordinals = Vec::new();
            let mut exceeded = false;
            self.visit_numeric(column, |ordinal, value| {
                if let Some(count) = exact.get_mut(&value) {
                    *count = count.saturating_add(1);
                    if !exceeded {
                        if ordinals.len() < FREQUENCY_ORDINALS {
                            ordinals.push(ordinal);
                        } else {
                            ordinals.clear();
                            exceeded = true;
                        }
                    }
                }
            })?;
            (exact, ordinals)
        };
        let mut entries = exact
            .into_iter()
            .map(|(value, count)| FrequencyEntry { value, count })
            .collect::<Vec<_>>();
        let omitted_max = keep_most_frequent(&mut entries).max(decrements);
        Ok(Some(FrequencySummary { entries, omitted_max, ordinals }))
    }

    fn visit_numeric(
        &self,
        column: usize,
        mut visit: impl FnMut(u64, FrequencyValue),
    ) -> Result<()> {
        let ty = &self.table.fields[column].ty;
        let mut start = 0_u64;
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
                // row at a time: frequency construction visits decoded values to update bounded candidates.
                for row in 0..rows {
                    let value = if vector.is_null_at(row) {
                        FrequencyValue::Null
                    } else {
                        // An unsigned column has no signed reading, and the documented fallback is
                        // the value itself. Every unsigned width the format stores fits in the
                        // `i128` a candidate is keyed by, so nothing is lost on the way through.
                        let widened = match vector.signed_at(row) {
                            Some(value) => Some(value),
                            None => match vector.value_at(row) {
                                Value::UTinyInt(value) => Some(i128::from(value)),
                                Value::USmallInt(value) => Some(i128::from(value)),
                                Value::UInteger(value) => Some(i128::from(value)),
                                Value::UBigInt(value) => Some(i128::from(value)),
                                _ => None,
                            },
                        };
                        FrequencyValue::Integer(widened.ok_or_else(|| {
                            invalid("numeric frequency page did not contain an integer value")
                        })?)
                    };
                    visit(start.saturating_add(row as u64), value);
                }
                start = start.saturating_add(rows as u64);
            }
        }
        Ok(())
    }

    /// Builds independent numeric synopses concurrently after all column pages are committed.
    ///
    /// The columns go through a queue rather than being cut into equal runs, because they are not
    /// equally expensive and they are not shuffled. A `BIGINT` column carries eight times the bytes
    /// of a `TINYINT` through the decode, and a run of them sits together in a schema the way it
    /// sits together in `hits`, so a worker that was handed the wrong six columns finishes long
    /// after one that was handed the right six and the whole phase waits for it.
    fn numeric_frequencies(&self) -> Result<Vec<Option<FrequencySummary>>> {
        let mut columns = self
            .table
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
            .collect::<Vec<_>>();
        let workers = std::thread::available_parallelism()
            .map_or(1, usize::from)
            .min(MAX_FREQUENCY_WORKERS)
            .min(columns.len());
        if workers <= 1 {
            let mut frequencies = vec![None; self.table.fields.len()];
            for column in columns {
                frequencies[column] = self.numeric_frequency(column)?;
            }
            return Ok(frequencies);
        }
        // Popped from the back, so the expensive columns are the ones taken first and the cheap ones
        // are what is left to fill in behind them.
        columns.sort_by_key(|&column| weight(&self.table.fields[column].ty));
        let queue = Mutex::new(columns);
        let pieces = std::thread::scope(|scope| {
            (0..workers)
                .map(|_| {
                    scope.spawn(|| {
                        let mut mine = Vec::new();
                        loop {
                            let taken = queue
                                .lock()
                                .map_err(|_| Error::internal("a native frequency worker panicked"))?
                                .pop();
                            let Some(column) = taken else { break };
                            mine.push((column, self.numeric_frequency(column)?));
                        }
                        Ok(mine)
                    })
                })
                .collect::<Vec<_>>()
                .into_iter()
                .map(|handle| {
                    handle
                        .join()
                        .map_err(|_| Error::internal("a native frequency worker panicked"))?
                })
                .collect::<Result<Vec<_>>>()
        })?;
        let mut frequencies = vec![None; self.table.fields.len()];
        for piece in pieces {
            for (column, summary) in piece {
                frequencies[column] = summary;
            }
        }
        Ok(frequencies)
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
        self.flush_pending()?;
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
        self.table.frequencies = self.numeric_frequencies()?;
        let dictionaries = std::mem::take(&mut self.dictionaries);
        let orders = rankings(&dictionaries)?;
        for (index, (dictionary, order)) in dictionaries.into_iter().zip(orders).enumerate() {
            let Some(dictionary) = dictionary else { continue };
            // A code nothing counted is a code no non-null row of this column holds, which is the
            // empty string a null was written as and nothing else, because a code is only ever made
            // by a row asking for one.
            self.table.distincts[index] =
                Some(dictionary.counts.iter().filter(|count| **count != 0).count() as u64);
            self.table.frequencies[index] = Some(code_frequency(&dictionary));
            let encoded = encode_global_dictionary(dictionary, &order)?;
            let offset = self.at;
            self.put(&encoded.index)?;
            self.put(&encoded.ranks)?;
            for block in &encoded.payload {
                self.put(block)?;
            }
            let payload_len =
                encoded.payload.iter().try_fold(0_usize, |len, block| len.checked_add(block.len()));
            let length = payload_len
                .and_then(|len| len.checked_add(encoded.index.len()))
                .and_then(|len| len.checked_add(encoded.ranks.len()))
                .ok_or_else(|| invalid("dictionary page length overflow"))?;
            self.table.dictionaries[index] = Some(Page {
                offset,
                length: u32::try_from(length)
                    .map_err(|_| invalid("dictionary page length overflow"))?,
                hash: checksum(&encoded.index),
            });
        }
        self.write_stats()?;
        let directory = encode_directory(&self.table)?;
        if directory.len() > MAX_DIRECTORY {
            return Err(invalid("directory exceeds the configured bound"));
        }
        let offset = self.at;
        self.put(&directory)?;
        Ok(Entry {
            name: self.table.name.clone(),
            fields: self.table.fields.clone(),
            rows: self.table.rows,
            directory: Page {
                offset,
                length: u32::try_from(directory.len())
                    .map_err(|_| invalid("directory length overflow"))?,
                hash: checksum(&directory),
            },
        })
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
                    &self.file,
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
        let mut tables = std::mem::take(&mut self.closed);
        tables.push(entry);
        let catalog = encode_catalog(&tables, &self.views)?;
        if catalog.len() > MAX_DIRECTORY {
            return Err(invalid("catalog exceeds the configured bound"));
        }
        let offset = self.at;
        self.put(&catalog)?;
        // Every page and every table directory is on the disk before anything points at them. The
        // slot write below is what makes this generation the one a reader picks, so the order of
        // these two syncs is the whole of the commit.
        self.file.sync_all().map_err(io)?;
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
        write_at(&self.file, slot_offset(self.generation), &slot.bytes())?;
        self.file.sync_all().map_err(io)?;
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
        let path = path.as_ref();
        let (_, size, slot, bytes, _) = slot_bytes(path)?;
        let (closed, _) = decode_catalog(&bytes, size)?;
        let generation = slot
            .generation
            .checked_add(1)
            .ok_or_else(|| invalid("native file generation overflow"))?;
        let catalog = encode_catalog(&closed, views)?;
        if catalog.len() > MAX_DIRECTORY {
            return Err(invalid("catalog exceeds the configured bound"));
        }
        let file = OpenOptions::new().write(true).read(true).open(path).map_err(io)?;
        write_at(&file, size, &catalog)?;
        file.sync_all().map_err(io)?;
        let slot = Slot {
            offset: size,
            length: u32::try_from(catalog.len()).map_err(|_| invalid("catalog length overflow"))?,
            generation,
            hash: checksum(&catalog),
        };
        write_at(&file, slot_offset(generation), &slot.bytes())?;
        file.sync_all().map_err(io)?;
        Ok(())
    }
}

/// Appends one run of bytes at `at` and moves it past them, answering where they went.
///
/// The append half of [`attach`], which cannot use [`Writer::put`] because it is not writing a
/// table. Every byte a section costs goes through here, so the offsets in an extent table come
/// from one place.
fn append(file: &File, at: &mut u64, bytes: &[u8]) -> Result<u64> {
    let offset = *at;
    write_at(file, offset, bytes)?;
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
    file: &File,
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
    for chunk in one.bytes.chunks(section::MAX_EXTENT as usize) {
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
    let path = path.as_ref();
    let (_, size, slot, bytes, _) = slot_bytes(path)?;
    let (mut entries, views) = decode_catalog(&bytes, size)?;
    let at = entries
        .iter()
        .position(|entry| entry.name == table)
        .ok_or_else(|| invalid(&format!("the file holds no table called {table}")))?;
    let file = OpenOptions::new().write(true).read(true).open(path).map_err(io)?;
    let mut version = [0; 4];
    read_at(&file, 8, &mut version)?;
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
    read_at(&file, entries[at].directory.offset, &mut directory)?;
    if checksum(&directory) != entries[at].directory.hash {
        return Err(invalid(&format!("the directory of table {table} does not checksum")));
    }
    let mut held = decode_directory(&directory, size)?;
    let mut cursor = size;
    for one in attachments {
        let written = write_section(&file, &mut cursor, one, held.generation)?;
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
    let offset = append(&file, &mut cursor, &encoded)?;
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
    let offset = append(&file, &mut cursor, &catalog)?;
    file.sync_all().map_err(io)?;
    let generation =
        slot.generation.checked_add(1).ok_or_else(|| invalid("native file generation overflow"))?;
    let committed = Slot {
        offset,
        length: u32::try_from(catalog.len()).map_err(|_| invalid("catalog length overflow"))?,
        generation,
        hash: checksum(&catalog),
    };
    write_at(&file, slot_offset(generation), &committed.bytes())?;
    file.sync_all().map_err(io)?;
    Ok(held)
}

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
    cache: Arc<Vec<Mutex<Cached>>>,
    /// How many whole stripe pages have been read, which is what the sharing above is judged on. A
    /// scan of a column should read each of its stripes once however many workers it has.
    pages: Arc<AtomicUsize>,
    /// How many index sections have been read. A scan of a column should read each of its stripes
    /// once here too, and the test that says so is the only thing keeping it that way.
    indexes: Arc<AtomicUsize>,
    /// How many stripes of one column the page cache keeps. See [`CACHED_STRIPES_PER_COLUMN`] for
    /// what sets it and [`Reader::keep_stripes`] for who raises it.
    kept: Arc<AtomicUsize>,
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
    page: Option<Arc<Vec<u8>>>,
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
#[derive(Debug, Default)]
struct Cached {
    pages: Vec<Option<Arc<Vec<u8>>>>,
    order: VecDeque<usize>,
    loading: Vec<usize>,
    index: Vec<Option<Arc<Vec<PartSpan>>>>,
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
    offsets: Vec<u8>,
    /// Bits one offset is packed at, which is what the largest block of this column spans and is the
    /// same for every block of it.
    offset_bits: usize,
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
    payload: u64,
    /// Where each block of the payload ends in the file, as a byte offset from `payload`. The
    /// blocks are stored back to back, so a block starts where the one before it ended.
    ends: Vec<u64>,
    hashes: Vec<u64>,
    /// The payload, read and decoded a block at a time and kept after that.
    blocks: Vec<OnceLock<Result<Vec<u8>>>>,
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

    /// Reads and decodes one block of the payload, without deciding who keeps it.
    ///
    /// [`Self::payload_block`] keeps it forever, which is what a point read wants and what a walk
    /// of the whole dictionary must not do. Both call this and they differ in nothing else.
    fn decode_block(&self, block: usize) -> Result<Vec<u8>> {
        let start = if block == 0 { 0 } else { self.ends[block - 1] };
        let end = self.ends[block];
        let len = end
            .checked_sub(start)
            .ok_or_else(|| invalid("global dictionary block ends before it starts"))?;
        let mut stored = vec![
            0;
            usize::try_from(len).map_err(|_| invalid(
                "global dictionary block does not fit in memory"
            ))?
        ];
        read_at(&self.file, self.payload + start, &mut stored)?;
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

    /// Where the value at `index` ends inside its payload block.
    fn end_within(&self, index: usize) -> Result<u32> {
        let run = index / TEXT_OFFSET_RUN;
        let bytes = self
            .offsets
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
    /// [`bitpack::unpack_tail`] walks the run instead, which makes the window a fixed width and so
    /// an unaligned load, and reads the bit position off a counter. A run is five hundred and twelve
    /// values and a block is two of them, so a block of a thousand and twenty four values costs two
    /// calls here and nothing per value.
    fn ends_within(&self, first: usize, last: usize) -> Result<Vec<u64>> {
        let mut ends = Vec::with_capacity(last.saturating_sub(first));
        let mut at = first;
        while at < last {
            let run = at / TEXT_OFFSET_RUN;
            let stop = ((run + 1) * TEXT_OFFSET_RUN).min(last);
            let held = self.values.saturating_sub(run * TEXT_OFFSET_RUN).min(TEXT_OFFSET_RUN);
            let bytes = self
                .offsets
                .get(run * TEXT_OFFSET_RUN / 8 * self.offset_bits..)
                .ok_or_else(|| invalid("global dictionary offsets are short"))?;
            let run_ends = bitpack::unpack_tail(bytes, self.offset_bits, held)
                .map_err(|_| invalid("global dictionary offsets are short"))?;
            let within = run_ends
                .get(at % TEXT_OFFSET_RUN..stop - run * TEXT_OFFSET_RUN)
                .ok_or_else(|| invalid("global dictionary offsets are short"))?;
            ends.extend_from_slice(within);
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
        let within = index % TEXT_OFFSET_RUN;
        let (start, end) = if within == 0 {
            (self.start_within(index)?, self.end_within(index)?)
        } else {
            let run = index / TEXT_OFFSET_RUN;
            let bytes = self
                .offsets
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
                let which = rank / TEXT_RANK_BLOCK;
                let start = if which == 0 { 0 } else { self.rank_ends[which - 1] };
                let end = self.rank_ends[which];
                let mut bytes = vec![0; (end - start) as usize];
                read_at(&self.file, self.rank_at + start, &mut bytes)?;
                if checksum(&bytes)
                    != *self
                        .rank_hashes
                        .get(rank / TEXT_RANK_BLOCK)
                        .ok_or_else(|| invalid("global dictionary rank block has no checksum"))?
                {
                    return Err(invalid("global dictionary rank checksum differs"));
                }
                Ok(bytes)
            })
            .as_ref()
            .map_err(Clone::clone)?;
        Ok((block.as_slice(), rank % TEXT_RANK_BLOCK))
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
fn offset_width(offsets: &[u32]) -> usize {
    let values = offsets.len() - 1;
    let mut span = 0;
    for first in (0..values).step_by(TEXT_PAYLOAD_VALUES) {
        let last = (first + TEXT_PAYLOAD_VALUES).min(values);
        span = span.max(offsets[last] - offsets[first]);
    }
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
fn encode_offsets(offsets: &[u32], bits: usize, out: &mut Vec<u8>) -> Result<()> {
    let values = offsets.len() - 1;
    let mut run = Vec::with_capacity(TEXT_OFFSET_RUN);
    for first in (0..values).step_by(TEXT_OFFSET_RUN) {
        let last = (first + TEXT_OFFSET_RUN).min(values);
        let base = offsets[first / TEXT_PAYLOAD_VALUES * TEXT_PAYLOAD_VALUES];
        run.clear();
        run.extend((first..last).map(|value| u64::from(offsets[value + 1] - base)));
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
    /// So a sweep keeps what it decodes while the column is under [`TEXT_KEEP_BUDGET`] and drops it
    /// after that. A block already in hand is used where it is there and costs nothing either way.
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
        let decoded;
        let bytes: &[u8] = match self.blocks.get(block).and_then(OnceLock::get) {
            Some(Ok(kept)) => kept,
            _ if self.payload_kept.load(Atomic::Relaxed) < self.keep_budget => {
                let kept = self
                    .payload_block(block)?
                    .ok_or_else(|| invalid("global dictionary block is past the payload"))?;
                self.payload_kept.fetch_add(kept.len(), Atomic::Relaxed);
                kept
            }
            _ => {
                decoded = self.decode_block(block)?;
                &decoded
            }
        };
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
                for first in (0..self.ranks).step_by(TEXT_RANK_BLOCK) {
                    let (block, _) = self.rank_parts(first).ok()?;
                    let count = self.rank_block_len(first);
                    let codes = self.rank_codes(block, count).ok()?;
                    for (within, code) in bitpack::unpack_tail(codes, self.code_bits, count)
                        .ok()?
                        .into_iter()
                        .enumerate()
                    {
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
            + self.hashes.capacity() * size_of::<u64>()
            + self.ends.capacity() * size_of::<u64>()
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
fn read_index(file: &File, stripe: &Stripe, column: usize) -> Result<Vec<PartSpan>> {
    let parts = stripe.parts.len();
    let section = index_section(parts)?;
    let at = column.checked_mul(section).ok_or_else(|| invalid("index page offset overflow"))?;
    let end = at.checked_add(section).ok_or_else(|| invalid("index page offset overflow"))?;
    if end > stripe.index.length as usize {
        return Err(invalid("index page is shorter than its columns"));
    }
    let page = stripe.pages.get(column).ok_or_else(|| invalid("stripe page is missing"))?;
    let mut bytes = vec![0; section];
    let offset = stripe
        .index
        .offset
        .checked_add(at as u64)
        .ok_or_else(|| invalid("index page offset overflow"))?;
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

/// Puts one stripe of one column in the cache, dropping the stripe that has been there longest.
///
/// The index goes in its own slot and stays. Only the page is under the budget, and `kept` is how
/// many pages that budget is.
fn remember(cached: &mut Cached, held: &CachedColumn, kept: usize) {
    if let Some(slot) = cached.index.get_mut(held.stripe) {
        if slot.is_none() {
            *slot = Some(Arc::clone(&held.index));
        }
    }
    let Some(page) = held.page.clone() else { return };
    let Some(slot) = cached.pages.get_mut(held.stripe) else { return };
    if slot.is_none() {
        cached.order.push_back(held.stripe);
    }
    *slot = Some(page);
    while cached.order.len() > kept.max(1) {
        let Some(oldest) = cached.order.pop_front() else { break };
        if let Some(slot) = cached.pages.get_mut(oldest) {
            *slot = None;
        }
    }
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
}

impl Catalog {
    /// Reads the highest valid catalog slot and nothing under it.
    ///
    /// # Errors
    ///
    /// If the file has no valid committed catalog or a catalog pointer is out of bounds.
    pub fn open(path: impl AsRef<Path>) -> Result<Self> {
        let (file, size, _, bytes, opening) = slot_bytes(path)?;
        let (entries, views) = decode_catalog(&bytes, size)?;
        Ok(Self {
            file: Arc::new(file),
            size,
            entries: Arc::new(entries),
            views: Arc::new(views),
            opening,
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
        let mut bytes = vec![0; entry.directory.length as usize];
        read_at(&self.file, entry.directory.offset, &mut bytes)?;
        if checksum(&bytes) != entry.directory.hash {
            return Err(invalid(&format!("the directory of table {name} does not checksum")));
        }
        let mut opening = self.opening;
        opening.reads += 1;
        opening.bytes += u64::from(entry.directory.length);
        Reader::build(
            Arc::clone(&self.file),
            self.size,
            decode_directory(&bytes, self.size)?,
            u64::from(entry.directory.length),
            opening,
        )
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
    let mut file = File::open(path).map_err(io)?;
    let size = file.metadata().map_err(io)?.len();
    if size < HEADER {
        return Err(invalid("file is shorter than its header"));
    }
    let mut header = [0; HEADER as usize];
    file.read_exact(&mut header).map_err(io)?;
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
        file.seek(SeekFrom::Start(slot.offset)).map_err(io)?;
        file.read_exact(&mut bytes).map_err(io)?;
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
    Ok((file, size, slot, bytes, opening))
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
    ) -> Result<Self> {
        let places = places(&table)?;
        let dictionaries = (0..table.fields.len()).map(|_| OnceLock::new()).collect();
        let table_fields = table.fields.len();
        let stripes = table.stripes.len();
        let cache = (0..table.fields.len())
            .map(|_| {
                Mutex::new(Cached {
                    pages: (0..stripes).map(|_| None).collect(),
                    index: (0..stripes).map(|_| None).collect(),
                    ..Cached::default()
                })
            })
            .collect::<Vec<_>>();
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
            opened: Arc::new(AtomicUsize::new(0)),
            sieves: Arc::new(sieves),
            part_ranges: Arc::new(part_ranges),
            places: Arc::new(places),
            cache: Arc::new(cache),
            pages: Arc::new(AtomicUsize::new(0)),
            indexes: Arc::new(AtomicUsize::new(0)),
            kept: Arc::new(AtomicUsize::new(CACHED_STRIPES_PER_COLUMN)),
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
                memberships: sum(stripes.iter().map(|stripe| page_bytes(&stripe.memberships, at))),
                sieves: sum(stripes.iter().map(|stripe| page_bytes(&stripe.sieves, at))),
                part_ranges: sum(stripes.iter().map(|stripe| page_bytes(&stripe.part_ranges, at))),
                dictionary: page_bytes(&table.dictionaries, at),
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
        self.kept.fetch_max(stripes, Atomic::Relaxed);
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
        let Some(summary) = self.table.frequencies.get(column).and_then(Option::as_ref) else {
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
        let Some(summary) = self.table.frequencies.get(column).and_then(Option::as_ref) else {
            return Ok(None);
        };
        let entries = self.decode_frequencies(column, &field.ty, &summary.entries)?;
        Ok(Some(FrequencyPrefix { entries, omitted_max: summary.omitted_max }))
    }

    /// Turns stored frequency entries into values of the column's own type.
    fn decode_frequencies(
        &self,
        column: usize,
        ty: &LogicalType,
        entries: &[FrequencyEntry],
    ) -> Result<Vec<(Value, u64)>> {
        let dictionary = if *ty == LogicalType::Varchar { self.dictionary(column)? } else { None };
        let mut out = Vec::with_capacity(entries.len());
        for entry in entries {
            let value = match entry.value {
                FrequencyValue::Null => Value::Null,
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
                FrequencyValue::Code(code) => dictionary
                    .as_ref()
                    .ok_or_else(|| invalid("frequency code has no dictionary"))?
                    .try_value_at(code as usize)?,
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
        self.table
            .fields
            .get(column)
            .ok_or_else(|| invalid("frequency column index out of range"))?;
        let Some(summary) = self.table.frequencies.get(column).and_then(Option::as_ref) else {
            return Ok(None);
        };
        if summary.ordinals.is_empty() {
            return Ok(None);
        }
        Ok(Some(FrequencyOccurrences {
            omitted_max: summary.omitted_max,
            ordinals: summary.ordinals.clone(),
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
    /// `None` for a column the file has no dictionary for, which is every column that is not a
    /// string. A sketch would answer that approximately and SQL asked for the exact number.
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
        if self.null_count(column)? > 0 {
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
        self.read_impl(part, columns, true)
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
        self.read_impl(part, columns, false)
    }

    /// Whether an exact global-code membership index proves that the stripe holding a part cannot
    /// contain any of the sorted candidate codes.
    ///
    /// # Errors
    ///
    /// If the part, column, index page, checksum, or delta stream is invalid.
    pub fn skips_codes(&self, part: usize, column: usize, candidates: &[u32]) -> Result<bool> {
        if candidates.is_empty() {
            return Ok(true);
        }
        if candidates.windows(2).any(|pair| pair[0] >= pair[1]) {
            return Err(Error::internal("native code candidates are not sorted and unique"));
        }
        let stripe = self.stripe_of(part)?;
        let Some(page) = stripe.memberships.get(column).copied().flatten() else {
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
        let cache = self.cache.get(column).ok_or_else(|| invalid("column index out of range"))?;
        let mut cached = cache.lock().map_err(|_| invalid("column page cache is poisoned"))?;
        let known = cached.index.get(at).and_then(Clone::clone);
        let page = cached.pages.get(at).and_then(Clone::clone);
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
            remember(&mut cached, &held, self.kept.load(Atomic::Relaxed));
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
        remember(&mut cached, &held, self.kept.load(Atomic::Relaxed));
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
            Some(Arc::new(bytes))
        } else {
            None
        };
        Ok(CachedColumn { stripe: at, index, page })
    }

    fn read_impl(&self, at: usize, columns: &[usize], whole: bool) -> Result<Chunk> {
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
                Some(held) => part_bytes(held, span)?,
                None => {
                    let offset = page
                        .offset
                        .checked_add(span.start as u64)
                        .ok_or_else(|| invalid("part range overflow"))?;
                    let mut bytes = vec![0; span.length];
                    read_at(&self.file, offset, &mut bytes)?;
                    owned = bytes;
                    &owned
                }
            };
            if checksum(bytes) != span.hash {
                return Err(invalid(&format!(
                    "column page checksum differs, column {column} part {} at {}+{} of {} bytes, \
                     wanted {:016x} and got {:016x}",
                    place.part,
                    page.offset,
                    span.start,
                    span.length,
                    span.hash,
                    checksum(bytes),
                )));
            }
            let dictionary = self.dictionary(column)?;
            // Held as a page, because a column that came out of a file is handed out more than
            // once. A group by clones its key columns out of the chunk so the keys outlive it, a
            // projection of a bare column name does the same, and a cut of a flat run copies unless
            // the run is a page. One `Arc` per column per part buys all of those, and it moves the
            // run into the `Arc` without touching a value.
            picked.push(decode(&field.ty, rows, bytes, dictionary)?.into_pages());
        }
        Chunk::with_rows(picked, rows)
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
        let page = self.table.stripes.get(stripe)?.part_ranges.get(column).copied().flatten()?;
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
        let page = self.table.stripes.get(stripe)?.sieves.get(column).copied().flatten()?;
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
    let text = dictionary
        .try_text_at(code)?
        .ok_or_else(|| invalid("global dictionary order names a code it does not have"))?;
    Ok(Value::Varchar(text.into()))
}

/// Writes one span of a file at an offset, without depending on where the cursor is.
///
/// The writer owns an offset of its own and passes it in here, so that nothing it writes depends on
/// a cursor that a read is entitled to move. Both of these can come back short and both loop.
#[cfg(unix)]
fn write_at(file: &File, mut offset: u64, mut bytes: &[u8]) -> Result<()> {
    use std::os::unix::fs::FileExt;
    while !bytes.is_empty() {
        let written = file.write_at(bytes, offset).map_err(io)?;
        if written == 0 {
            return Err(invalid("a write to the native file wrote nothing"));
        }
        offset += written as u64;
        bytes = &bytes[written..];
    }
    Ok(())
}

/// The same write, on the call Windows spells differently.
#[cfg(windows)]
fn write_at(file: &File, mut offset: u64, mut bytes: &[u8]) -> Result<()> {
    use std::os::windows::fs::FileExt;
    while !bytes.is_empty() {
        let written = file.seek_write(bytes, offset).map_err(io)?;
        if written == 0 {
            return Err(invalid("a write to the native file wrote nothing"));
        }
        offset += written as u64;
        bytes = &bytes[written..];
    }
    Ok(())
}

/// Somewhere that is neither, where the cursor is all there is.
#[cfg(not(any(unix, windows)))]
fn write_at(file: &File, offset: u64, bytes: &[u8]) -> Result<()> {
    use std::io::Write;
    let mut file = file.try_clone().map_err(io)?;
    file.seek(SeekFrom::Start(offset)).map_err(io)?;
    file.write_all(bytes).map_err(io)
}

/// Reads one span of a file at an offset, without moving a cursor anybody else can see.
///
/// Every reader of a table shares one [`File`] behind an [`Arc`], and a grouped aggregate reads its
/// pages from several threads at once, so this has to be positional. Seeking and then reading is
/// two calls with a gap in the middle, and in that gap another thread's seek lands and the read
/// comes back with somebody else's bytes.
///
/// Both of these can come back short, so both loop. A read of zero bytes before the span is filled
/// means the file stops earlier than the directory said it does.
#[cfg(unix)]
fn read_at(file: &File, mut offset: u64, mut bytes: &mut [u8]) -> Result<()> {
    use std::os::unix::fs::FileExt;
    while !bytes.is_empty() {
        let read = file.read_at(bytes, offset).map_err(io)?;
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
/// `seek_read` is one `ReadFile` carrying the offset with it, so two of them cannot interleave the
/// way a seek and a read can. It does leave the shared cursor somewhere afterwards, which is why
/// nothing in this file may read that cursor.
#[cfg(windows)]
fn read_at(file: &File, mut offset: u64, mut bytes: &mut [u8]) -> Result<()> {
    use std::os::windows::fs::FileExt;
    while !bytes.is_empty() {
        let read = file.seek_read(bytes, offset).map_err(io)?;
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
/// This one does race, and there is no way to write it so it does not. Nothing we build for runs
/// here, so it exists to keep the crate compiling rather than to be correct under threads.
#[cfg(not(any(unix, windows)))]
fn read_at(file: &File, offset: u64, bytes: &mut [u8]) -> Result<()> {
    let mut file = file.try_clone().map_err(io)?;
    file.seek(SeekFrom::Start(offset)).map_err(io)?;
    file.read_exact(bytes).map_err(io)
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

fn code_frequency(dictionary: &GlobalDictionary) -> FrequencySummary {
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
    FrequencySummary { entries, omitted_max, ordinals: Vec::new() }
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
    for dictionary in &table.dictionaries {
        match dictionary {
            None => out.push(0),
            Some(page) => {
                out.push(1);
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
        for ((field, dictionary), membership) in
            table.fields.iter().zip(&table.dictionaries).zip(&stripe.memberships)
        {
            if field.ty != LogicalType::Varchar || dictionary.is_none() {
                continue;
            }
            let page =
                membership.ok_or_else(|| invalid("string page has no code membership index"))?;
            put_u64(&mut out, page.offset);
            put_u32(&mut out, page.length);
            put_u64(&mut out, page.hash);
        }
        for sieve in &stripe.sieves {
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
        for held in &stripe.part_ranges {
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
        let Some(summary) = summary else {
            out.push(0);
            continue;
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
    let mut cur = Cursor { bytes, at: 0 };
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
        entries.push(Entry { name, fields, rows, directory });
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
    Ok((entries, views))
}

struct Cursor<'a> {
    bytes: &'a [u8],
    at: usize,
}
impl<'a> Cursor<'a> {
    fn take(&mut self, len: usize) -> Result<&'a [u8]> {
        let end = self.at.checked_add(len).ok_or_else(|| invalid("directory offset overflow"))?;
        let bytes =
            self.bytes.get(self.at..end).ok_or_else(|| invalid("directory is truncated"))?;
        self.at = end;
        Ok(bytes)
    }
    fn u8(&mut self) -> Result<u8> {
        Ok(self.take(1)?[0])
    }
    fn u16(&mut self) -> Result<u16> {
        Ok(u16::from_le_bytes(self.take(2)?.try_into().expect("two bytes")))
    }
    fn u32(&mut self) -> Result<u32> {
        Ok(u32::from_le_bytes(self.take(4)?.try_into().expect("four bytes")))
    }
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
    fn bound(&mut self) -> Result<Option<Bound>> {
        bounds::get(self.bytes, &mut self.at)
    }
    fn text(&mut self) -> Result<String> {
        let len = self.u16()? as usize;
        String::from_utf8(self.take(len)?.to_vec()).map_err(|_| invalid("name is not UTF-8"))
    }
    /// Whether everything has been read, which is how a section that an older file does not have at
    /// all is told from one that is there and empty.
    fn done(&self) -> bool {
        self.at >= self.bytes.len()
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

fn decode_directory(bytes: &[u8], size: u64) -> Result<Table> {
    let mut cur = Cursor { bytes, at: 0 };
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
    for _ in 0..width {
        dictionaries.push(match cur.u8()? {
            0 => None,
            1 => {
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
            if field.ty != LogicalType::Varchar || dictionaries[column].is_none() {
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
            memberships[column] = Some(page);
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
            memberships,
            sieves,
            part_ranges,
            zone: Zone::from_ranges(ranges),
        });
    }
    if total != rows {
        return Err(invalid("table row count differs from stripes"));
    }
    let frequencies = if cur.at == bytes.len() {
        vec![None; width]
    } else {
        if cur.take(8)? != FREQUENCIES {
            return Err(invalid("directory extension magic differs"));
        }
        if cur.u16()? as usize != width {
            return Err(invalid("frequency column count differs"));
        }
        let mut frequencies = Vec::with_capacity(width);
        for field in &fields {
            let summary = match cur.u8()? {
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
                                | (LogicalType::Varchar, FrequencyValue::Code(_))
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
                    Some(FrequencySummary { entries, omitted_max, ordinals })
                }
                _ => return Err(invalid("frequency summary tag differs")),
            };
            frequencies.push(summary);
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
    let mut seen_sections = false;
    // Zero until a section table says otherwise, which is what a format 22 table gets and what
    // makes every section stamp fail to match on one, because real generations start at one.
    let mut generation = 0;
    while cur.at != bytes.len() {
        let mut tag = [0u8; 8];
        tag.copy_from_slice(cur.take(8)?);
        if &tag == CLUSTERING {
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
        } else {
            return Err(invalid("directory extension magic differs"));
        }
    }
    if cur.at != bytes.len() {
        return Err(invalid("directory has trailing bytes"));
    }
    Ok(Table {
        name,
        fields,
        stripes,
        rows,
        dictionaries,
        distincts,
        frequencies,
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
        let keep: &[integer::Kind] = if depth == 0 {
            &[integer::Kind::Constant, integer::Kind::Packed, integer::Kind::Rle]
        } else {
            &[integer::Kind::Constant, integer::Kind::Packed]
        };
        let narrowed: Vec<integer::Kind> =
            offered.iter().copied().filter(|kind| keep.contains(kind)).collect();
        // The contract is a non empty subset, and a chunk that offers none of the three is a chunk
        // this has no opinion about rather than one that cannot be written.
        if narrowed.is_empty() { offered.to_vec() } else { narrowed }
    }
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
        let keep: &[integer::Kind] = if depth == 0 {
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
        };
        let narrowed: Vec<integer::Kind> =
            offered.iter().copied().filter(|kind| keep.contains(kind)).collect();
        if narrowed.is_empty() { offered.to_vec() } else { narrowed }
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
) -> Result<Option<Vec<u8>>> {
    let (Some(width), Some(data)) = (plain_width(ty), flat.data()) else { return Ok(None) };
    let Some(values) = widened(data) else { return Ok(None) };
    let plain = values.len().saturating_mul(width);
    let best = match packed {
        // The tag, the base, the word count and the words, which is what the codec 2 branch writes.
        Some(packed) => plain.min(21 + size_of_val(packed.words())),
        None => plain,
    };
    let out = integer::encode_with(&values, &Fixed)?;
    Ok((out.len() < best).then_some(out))
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
        let text = flat.text_at(row).unwrap_or("").as_bytes();
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

fn encode(
    vector: &Vector,
    global: Option<&mut GlobalDictionary>,
) -> Result<(Vec<u8>, Option<Vec<u32>>)> {
    let ty = vector.logical_type();
    // flatten: the file writer needs a uniform scalar page and does it once per loaded chunk.
    let flat = vector.flatten()?;
    let mut out = Vec::new();
    let mut global_codes = None;
    if let Some(global) = global {
        let mut codes = Vec::with_capacity(flat.len());
        for row in 0..flat.len() {
            let text = flat.text_at(row).unwrap_or("");
            let code = global.code(text)?;
            global.observe(code, flat.is_null_at(row))?;
            codes.push(code);
        }
        global_codes = Some(codes);
    }
    let membership = global_codes.as_deref().map(unique_codes);
    let dictionary = if global_codes.is_none() && ty == &LogicalType::Varchar {
        string_dictionary(&flat)?
    } else {
        None
    };
    let compressed_text =
        if global_codes.is_none() && dictionary.is_none() && ty == &LogicalType::Varchar {
            text_compressed(&flat)?
        } else {
            None
        };
    let packed_vector = if dictionary.is_none() && global_codes.is_none() {
        Some(flat.bit_packed()?)
    } else {
        None
    };
    let packed = packed_vector.as_ref().and_then(Vector::packed_parts);
    let coded = match global_codes.as_deref() {
        Some(codes) => encoded_codes(codes)?,
        None => None,
    };
    // Only where nothing else has claimed the page, which is the plain integer case. A packed part
    // is still on the table because the cascade has to beat it too: the bit pack takes a part only
    // when it halves it, so a column that shrinks by a third was coming out whole.
    let cascade = if dictionary.is_none() && global_codes.is_none() {
        cascaded(&flat, ty, packed.as_ref())?
    } else {
        None
    };
    out.push(if coded.is_some() {
        4
    } else if cascade.is_some() {
        5
    } else if global_codes.is_some() {
        3
    } else if dictionary.is_some() {
        1
    } else if compressed_text.is_some() {
        6
    } else if packed.is_some() {
        2
    } else {
        0
    });
    let nulls = flat.validity();
    let flag = match nulls {
        Validity::AllValid => 0,
        Validity::AllInvalid => 1,
        Validity::Mask(_) => 2,
    };
    out.push(flag);
    if flag == 2 {
        for group in (0..vector.len()).step_by(8) {
            let mut bits = 0_u8;
            for bit in 0..8 {
                if group + bit < vector.len() && !flat.is_null_at(group + bit) {
                    bits |= 1 << bit;
                }
            }
            out.push(bits);
        }
    }
    if let Some(coded) = coded {
        out.extend_from_slice(&coded);
        return Ok((out, membership));
    }
    if let Some(cascade) = cascade {
        out.extend_from_slice(&cascade);
        return Ok((out, membership));
    }
    if let Some(codes) = global_codes {
        for code in codes {
            put_u32(&mut out, code);
        }
        return Ok((out, membership));
    }
    if let Some(dictionary) = dictionary {
        out.extend_from_slice(&dictionary);
        return Ok((out, membership));
    }
    if let Some(compressed_text) = compressed_text {
        out.extend_from_slice(&compressed_text);
        return Ok((out, membership));
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
        return Ok((out, membership));
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
    Ok((out, membership))
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
    let mut cur = Cursor { bytes, at: 0 };
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
        let text = vector.text_at(row).unwrap_or("");
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
        out.extend_from_slice(value.as_bytes());
    }
    for code in codes {
        put_u32(&mut out, code);
    }
    Ok(Some(out))
}

struct EncodedDictionary {
    index: Vec<u8>,
    ranks: Vec<u8>,
    /// The payload as the blocks it is written as, kept apart rather than joined because joining
    /// them is a second copy of a thing that is already gigabytes on the columns that matter.
    payload: Vec<Vec<u8>>,
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

/// The first eight bytes of a value as an integer that sorts the way the bytes sort.
fn head(bytes: &[u8]) -> u64 {
    let mut word = [0; 8];
    let take = bytes.len().min(8);
    word[..take].copy_from_slice(&bytes[..take]);
    u64::from_be_bytes(word)
}

/// The sorted order of every global dictionary, one entry per column and empty where there is no
/// dictionary.
///
/// One column's sort has nothing to do with another's, and a table like `hits` has fifteen string
/// columns, so this runs across threads the way the numeric synopses above do. It is the only part
/// of committing a file that is more than bookkeeping, and doing it serially would show up as a
/// pause at the end of a load that thirty two threads had been busy with until then.
fn rankings(dictionaries: &[Option<GlobalDictionary>]) -> Result<Vec<Vec<(u64, u32)>>> {
    let present =
        dictionaries.iter().enumerate().filter(|(_, held)| held.is_some()).map(|(at, _)| at);
    let present = present.collect::<Vec<_>>();
    let mut orders = vec![Vec::new(); dictionaries.len()];
    let workers = std::thread::available_parallelism()
        .map_or(1, usize::from)
        .min(MAX_FREQUENCY_WORKERS)
        .min(present.len());
    if workers <= 1 {
        for at in present {
            if let Some(dictionary) = &dictionaries[at] {
                orders[at] = dictionary.ranked();
            }
        }
        return Ok(orders);
    }
    let width = present.len().div_ceil(workers);
    let pieces = std::thread::scope(|scope| {
        present
            .chunks(width)
            .map(|columns| {
                scope.spawn(|| {
                    columns
                        .iter()
                        .filter_map(|&at| dictionaries[at].as_ref().map(|held| (at, held.ranked())))
                        .collect::<Vec<_>>()
                })
            })
            .collect::<Vec<_>>()
            .into_iter()
            .map(|handle| {
                handle.join().map_err(|_| Error::internal("a dictionary sort worker panicked"))
            })
            .collect::<Result<Vec<_>>>()
    })?;
    for piece in pieces {
        for (at, order) in piece {
            orders[at] = order;
        }
    }
    Ok(orders)
}

fn encode_global_dictionary(
    dictionary: GlobalDictionary,
    order: &[(u64, u32)],
) -> Result<EncodedDictionary> {
    let values = dictionary.offsets.len() - 1;
    if order.len() != values {
        return Err(invalid("global dictionary order does not cover its values"));
    }
    let blocks = values.div_ceil(TEXT_PAYLOAD_VALUES);
    let payload = encode_payload(&dictionary)?;
    if payload.len() != blocks {
        return Err(invalid("global dictionary payload is not the blocks it says it is"));
    }
    let (ranks, rank_ends) = encode_ranks(order, code_width(values))?;
    let rank_blocks = values.div_ceil(TEXT_RANK_BLOCK);
    let offset_bits = offset_width(&dictionary.offsets);
    let mut index = Vec::with_capacity(
        DICTIONARY_HEADER + offset_bytes(values, offset_bits) + (blocks + rank_blocks) * 16,
    );
    put_u32(
        &mut index,
        u32::try_from(values).map_err(|_| invalid("global dictionary has too many values"))?,
    );
    put_u32(&mut index, TEXT_PAYLOAD_VALUES as u32);
    put_u32(
        &mut index,
        u32::try_from(blocks).map_err(|_| invalid("global dictionary has too many blocks"))?,
    );
    put_u32(&mut index, offset_bits as u32);
    encode_offsets(&dictionary.offsets, offset_bits, &mut index)?;
    // Where each block ends, so a reader can find one. The stored blocks are shorter than the
    // decoded ones and by a different amount each, so this is the one thing the offsets above no
    // longer say.
    let mut at = 0_u64;
    for block in &payload {
        at = at
            .checked_add(block.len() as u64)
            .ok_or_else(|| invalid("global dictionary payload overflow"))?;
        put_u64(&mut index, at);
    }
    for block in &payload {
        put_u64(&mut index, checksum(block));
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
    Ok(EncodedDictionary { index, ranks, payload })
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

/// The payload as encoded blocks of [`TEXT_PAYLOAD_VALUES`] values each.
///
/// Across threads because this is the only part of committing a file that is real work rather than
/// bookkeeping. The blocks are the same size and cost about the same, so an index each is enough of
/// a queue and there is nothing to weight the way the numeric synopses are weighted.
fn encode_payload(dictionary: &GlobalDictionary) -> Result<Vec<Vec<u8>>> {
    let values = dictionary.offsets.len() - 1;
    let blocks = values.div_ceil(TEXT_PAYLOAD_VALUES);
    let run = |block: usize| {
        let first = block * TEXT_PAYLOAD_VALUES;
        let last = (first + TEXT_PAYLOAD_VALUES).min(values);
        (first..last)
            .map(|value| {
                let from = dictionary.offsets[value] as usize;
                let to = dictionary.offsets[value + 1] as usize;
                &dictionary.payload[from..to]
            })
            .collect::<Vec<_>>()
    };
    // A dictionary small enough to be the sample is small enough to search in full, and searching
    // it costs less than deciding not to.
    let shape = (blocks > PAYLOAD_SAMPLE_BLOCKS).then(|| settle_shape(&run, blocks)).transpose()?;
    let one = |block: usize| match &shape {
        Some(shape) => string::encode_with(&run(block), shape),
        None => string::encode(&run(block)),
    };
    let workers = std::thread::available_parallelism()
        .map_or(1, usize::from)
        .min(MAX_FREQUENCY_WORKERS)
        .min(blocks);
    if workers <= 1 {
        return (0..blocks).map(one).collect();
    }
    let next = AtomicUsize::new(0);
    let pieces = std::thread::scope(|scope| {
        (0..workers)
            .map(|_| {
                scope.spawn(|| {
                    let mut mine = Vec::new();
                    loop {
                        let block = next.fetch_add(1, Atomic::Relaxed);
                        if block >= blocks {
                            break;
                        }
                        mine.push((block, one(block)?));
                    }
                    Ok(mine)
                })
            })
            .collect::<Vec<_>>()
            .into_iter()
            .map(|handle| {
                handle.join().map_err(|_| Error::internal("a dictionary encode worker panicked"))?
            })
            .collect::<Result<Vec<_>>>()
    })?;
    let mut payload = vec![Vec::new(); blocks];
    for piece in pieces {
        for (block, bytes) in piece {
            payload[block] = bytes;
        }
    }
    Ok(payload)
}

/// Which of [`payload_shapes`] comes out smallest over a sample of the blocks.
///
/// Every shape is encoded over the same sample and the smallest wins, which is the exhaustive
/// search moved up a level: over shapes of a column rather than over candidates of a chunk. The
/// sample is spread across the dictionary so that the first and last blocks are both in it, because
/// a dictionary written in first seen order has its common values at the front and its long tail at
/// the back, and those do not compress alike.
fn settle_shape<'a>(
    run: &dyn Fn(usize) -> Vec<&'a [u8]>,
    blocks: usize,
) -> Result<chooser::Settled> {
    let last = blocks - 1;
    let sample = (0..PAYLOAD_SAMPLE_BLOCKS)
        .map(|region| run(region * last / (PAYLOAD_SAMPLE_BLOCKS - 1)))
        .collect::<Vec<_>>();
    let mut best: Option<(chooser::Settled, usize)> = None;
    for shape in payload_shapes() {
        let mut size = 0;
        for block in &sample {
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
    if ty != &LogicalType::Varchar {
        return Err(invalid("global dictionary belongs to a non-string column"));
    }
    let mut header = [0; DICTIONARY_HEADER];
    read_at(&file, page.offset, &mut header)?;
    let count = u32::from_le_bytes(header[0..4].try_into().expect("four bytes")) as usize;
    let per_block = u32::from_le_bytes(header[4..8].try_into().expect("four bytes")) as usize;
    let blocks = u32::from_le_bytes(header[8..12].try_into().expect("four bytes")) as usize;
    let offset_bits = u32::from_le_bytes(header[12..16].try_into().expect("four bytes")) as usize;
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
    // Two words a payload block, one for where it ends in the file and one for its checksum, and the
    // same two a rank block.
    let hash_len = blocks
        .checked_add(rank_blocks)
        .and_then(|words| words.checked_mul(16))
        .ok_or_else(|| invalid("global dictionary block count overflow"))?;
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
    let offsets = index[DICTIONARY_HEADER..DICTIONARY_HEADER + offset_len].to_vec();
    let mut words = index[DICTIONARY_HEADER + offset_len..]
        .chunks_exact(8)
        .map(|part| u64::from_le_bytes(part.try_into().expect("eight bytes")))
        .collect::<Vec<_>>();
    let mut hashes = words.split_off(blocks);
    let mut rank_ends = hashes.split_off(blocks);
    let rank_hashes = rank_ends.split_off(rank_blocks);
    let ends = words;
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
    // What the offsets bound is the decoded payload, and what the page holds is the stored one, so
    // the last block end is the only thing that ties the index to the length of the page.
    let stored_len = page.length as usize - body_len;
    if ends.last().copied().unwrap_or_default() as usize != stored_len
        || ends.windows(2).any(|pair| pair[0] > pair[1])
    {
        return Err(invalid("global dictionary blocks do not bound the payload"));
    }
    Vector::external_text(
        LogicalType::Varchar,
        Arc::new(NativeText {
            file,
            values: count,
            offsets,
            offset_bits,
            ranks,
            rank_at: page.offset + index_len as u64,
            rank_ends,
            rank_hashes,
            rank_blocks: (0..rank_blocks).map(|_| OnceLock::new()).collect(),
            code_bits: code_width(count),
            code_ranks: OnceLock::new(),
            payload: page.offset + body_len as u64,
            ends,
            hashes,
            blocks: (0..blocks).map(|_| OnceLock::new()).collect(),
            keep_budget,
            payload_kept: AtomicUsize::new(0),
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
        let mut cur = Cursor { bytes, at: 0 };
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

fn decode(
    ty: &LogicalType,
    rows: usize,
    bytes: &[u8],
    global: Option<Arc<Vector>>,
) -> Result<Vector> {
    let mut cur = Cursor { bytes, at: 0 };
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
        if ty != &LogicalType::Varchar {
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
            strings.push_in_place(pair[0] as usize, (pair[1] - pair[0]) as usize)?;
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
        let dictionary = Vector::flat(LogicalType::Varchar, Data::Varlen(strings))?;
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
            // Converted in one pass and checked in the same one, rather than a fallible conversion
            // per code. A `Result` an element is a short circuit the loop cannot be vectorized past,
            // and it was costing about twelve instructions a row to narrow a number that already
            // fits. Every code a file holds is inside a `u32` or the file is corrupt, so the check
            // belongs once at the end: or the codes together and the answer has a bit set above the
            // low thirty two, or the sign bit, exactly when one of them did.
            let mut codes = Vec::with_capacity(wide.len());
            let mut seen = 0_i64;
            for &code in &wide {
                seen |= code;
                codes.push(code as u32);
            }
            if seen < 0 || seen > i64::from(u32::MAX) {
                return Err(invalid("code is not a code"));
            }
            codes
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
        if ty != &LogicalType::Varchar {
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
            values.push_in_place(start, len)?;
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
        let mut words = Vec::with_capacity(count);
        for _ in 0..count {
            words.push(cur.u64()?);
        }
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
    use std::fs;
    use std::io::{Seek, SeekFrom, Write};
    use std::path::PathBuf;
    use std::time::{SystemTime, UNIX_EPOCH};

    use rudb_common::Stat;
    use rudb_common::Value;
    use rudb_common::bounds::{Frequencies, Op, Remainder, Zones};
    use rudb_common::stat::Provenance;

    use super::*;

    #[test]
    fn checksum_matches_fixed_vectors() {
        assert_eq!(checksum(b""), 0xef46_db37_51d8_e999);
        assert_eq!(checksum(b"a"), 0xd24e_c4f1_a98c_6e5b);
        assert_eq!(checksum(b"abc"), 0x44bc_2cf5_ad77_0999);
    }

    fn path(label: &str) -> PathBuf {
        let stamp = SystemTime::now().duration_since(UNIX_EPOCH).expect("time advances").as_nanos();
        std::env::temp_dir().join(format!("rudb-native-{label}-{}-{stamp}.rdb", std::process::id()))
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

    /// The writer records where it put a page and puts it there, whatever the cursor is doing.
    ///
    /// The cursor is moved between the steps that record an offset, which is what reading the pages
    /// back to build the frequencies does on a platform with no `pread`. Without the fix the
    /// directory lands on top of a page and the file fails to reopen.
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
        writer.file.seek(SeekFrom::Start(0)).expect("the cursor goes back to the header");
        writer.append(&sample()).expect("second part");
        writer.file.seek(SeekFrom::Start(1)).expect("and somewhere useless again");
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
        let bits = u32::from_le_bytes(header[12..16].try_into().expect("four bytes")) as usize;
        let rank_blocks = count.div_ceil(TEXT_RANK_BLOCK as u64);
        DICTIONARY_HEADER as u64
            + offset_bytes(count as usize, bits) as u64
            + (blocks + rank_blocks) * 16
    }

    /// How long the sorted order is, which is where its last block ends.
    fn last_rank_end(file: &File, offset: u64, header: &[u8; DICTIONARY_HEADER]) -> u64 {
        let count = u64::from(u32::from_le_bytes(header[0..4].try_into().expect("four bytes")));
        let blocks = u64::from(u32::from_le_bytes(header[8..12].try_into().expect("four bytes")));
        let bits = u32::from_le_bytes(header[12..16].try_into().expect("four bytes")) as usize;
        let rank_blocks = count.div_ceil(TEXT_RANK_BLOCK as u64);
        let at = offset
            + DICTIONARY_HEADER as u64
            + offset_bytes(count as usize, bits) as u64
            + blocks * 16
            + (rank_blocks - 1) * 8;
        let mut end = [0; 8];
        read_at(file, at, &mut end).expect("the last rank block end");
        u64::from_le_bytes(end)
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
    fn the_planner_gets_a_row_count_per_value_off_a_complete_synopsis() {
        // The whole of the frequency half of #1106, end to end over a real file. Six rows, three
        // of one value and two of another, and a complete synopsis because six rows is well inside
        // what the writer can account for. The estimate for `id = 4` is three rows rather than a
        // sixth of the table, and for a value the file does not hold it is none.
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
        // Not in the file, and a synopsis that accounts for all six rows proves it.
        assert_eq!(
            common.rows_with(column, &Bound::Int(7)),
            Stat::exact(0, Provenance::FrequencySynopsis)
        );
        // A constant of another domain against an integer column. Nothing in the list compares
        // with it, so the zero above would be an artefact of the mismatch rather than a fact.
        assert_eq!(common.rows_with(column, &Bound::Bytes(b"four".to_vec())), Stat::Unknown);
        // A complete list has no remainder. Answering one of no rows over no values would hand the
        // caller a division to special case, and the counts above already answer this column.
        assert_eq!(common.remainder(column), None);
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
            distincts: vec![None],
            frequencies: vec![None],
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
            }],
            &[sample_view("items")],
        )
        .expect("it encodes, because encoding does not look");
        let error = decode_catalog(&bytes, HEADER + 8).expect_err("and decoding does");
        assert!(error.to_string().contains("same name"), "{error}");
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
        let integers = reader.top_frequencies(0, 1).expect("valid integer synopsis").expect("kept");
        assert_eq!(
            integers,
            vec![(Value::Integer(-2), 2), (Value::Integer(4), 2), (Value::Integer(9), 2),]
        );
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

        let page =
            Reader::open(&path).expect("reopen").table.stripes[0].sieves[0].expect("a sieve page");
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
        assert!(occurrences.ordinals.windows(2).all(|pair| pair[0] < pair[1]));
        assert_eq!(&occurrences.ordinals[..1_000], &(0_u64..1_000).collect::<Vec<_>>());
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
        let index_len = dictionary_index_len(&header);
        let rank_len = last_rank_end(&reader.file, dictionary.offset, &header);
        let mut file = OpenOptions::new().write(true).open(&path).expect("open dictionary page");
        file.seek(SeekFrom::Start(dictionary.offset + index_len + rank_len))
            .expect("inside dictionary payload");
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

        let mut file = OpenOptions::new().write(true).open(&path).expect("open dictionary page");
        file.seek(SeekFrom::Start(dictionary.offset + u64::from(dictionary.length) - 4))
            .expect("the last bytes of the page are payload");
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
        fs::remove_file(path).expect("remove scratch file");
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

    /// A sweep of the dictionary reads every value and keeps what it read, up to the budget.
    ///
    /// The point of the sweep is the resident size rather than the answer, so both are checked
    /// here. A dictionary this small is well under [`TEXT_KEEP_BUDGET`], so it keeps everything and
    /// a second sweep decodes nothing, which is what makes the second statement of a session asking
    /// the same question cost what it should. The ceiling is the other half of it and it has its own
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

        let resting = dictionary.footprint();
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
        let after = dictionary.footprint();
        assert!(after > resting, "a sweep under the budget keeps what it decoded");

        let read = (0..dictionary.len())
            .map(|code| dictionary.try_bytes_at(code).expect("read").expect("a value").to_vec())
            .collect::<Vec<_>>();
        assert_eq!(swept, read, "a sweep answers what a point read answers");
        assert_eq!(dictionary.footprint(), after, "a point read of a kept block decodes nothing");
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
        let membership = reader.table.stripes[0].memberships[1].expect("string membership");
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
            distincts: vec![None],
            frequencies: vec![None],
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
        let ranked = dictionary.ranked();
        assert_eq!(ranked.len(), values.len(), "one entry a distinct value");

        let seen = ranked
            .iter()
            .map(|&(_, code)| {
                String::from_utf8(dictionary.bytes(code).expect("a coded value").to_vec())
                    .expect("text in, text out")
            })
            .collect::<Vec<_>>();
        let mut wanted = values.clone();
        wanted.sort_unstable();
        assert_eq!(seen, wanted, "the order is the order the bytes give");

        for &(carried, code) in &ranked {
            let value = dictionary.bytes(code).expect("a coded value");
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
        assert!(empty.ranked().is_empty(), "nothing in, nothing out");

        let mut dictionary = GlobalDictionary::new();
        for value in ["pear", "apple", "", "apples", "app"] {
            dictionary.code(value).expect("a code for every value");
        }
        let seen = dictionary
            .ranked()
            .iter()
            .map(|&(_, code)| dictionary.bytes(code).expect("a coded value").to_vec())
            .collect::<Vec<_>>();
        let wanted: Vec<Vec<u8>> =
            [&b""[..], b"app", b"apple", b"apples", b"pear"].iter().map(|v| v.to_vec()).collect();
        assert_eq!(seen, wanted, "shorter first where one runs out inside another");
    }
}
