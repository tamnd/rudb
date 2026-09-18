//! Rudb's single-file columnar snapshot format.
//!
//! A committed directory names independently readable column pages. The first version handles
//! scalar columns and one table; the file header already has two generation slots so an unfinished
//! replacement directory cannot hide the last complete one.
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
use std::collections::HashMap;
use std::fs::{File, OpenOptions};
use std::io::{Read, Seek, SeekFrom, Write};
use std::mem::size_of;
use std::path::Path;
use std::sync::atomic::{AtomicUsize, Ordering as Atomic};
use std::sync::{Arc, Mutex, OnceLock};

use rudb_common::bounds::Bound;
use rudb_common::{Error, Field, LogicalType, Result, Value};
use rudb_storage::{Probe, Range, Zone};
use rudb_vector::string::StringColumn;
use rudb_vector::validity::Validity;
use rudb_vector::{Buffer, Chunk, Data, TextSource, Vector};

const MAGIC: &[u8; 8] = b"RUDBNV10";
const DIRECTORY: &[u8; 8] = b"RUDBDI10";
const FORMAT: u32 = 11;
const HEADER: u64 = 80;
const SLOT_BYTES: usize = 28;
const MAX_PAGE: usize = 256 * 1024 * 1024;
const MAX_DIRECTORY: usize = 128 * 1024 * 1024;
const FREQUENCIES: &[u8; 8] = b"RUDBFQ2\0";
const FREQUENCY_CANDIDATES: usize = 32_768;
const FREQUENCY_ENTRIES: usize = 512;
const FREQUENCY_BUILD_RANK: usize = 10;
const FREQUENCY_ORDINALS: usize = 65_536;
const MAX_FREQUENCY_WORKERS: usize = 16;

fn io(error: std::io::Error) -> Error {
    Error::io(error.to_string())
}

fn invalid(message: &str) -> Error {
    Error::invalid_input(format!("invalid rudb native file: {message}"))
}

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
    let word =
        |at: usize| u64::from_le_bytes(bytes[at..at + 8].try_into().expect("eight checksum bytes"));

    let mut at = 0;
    let mut hash = if bytes.len() >= 32 {
        let mut one = P1.wrapping_add(P2);
        let mut two = P2;
        let mut three = 0;
        let mut four = 0_u64.wrapping_sub(P1);
        while at + 32 <= bytes.len() {
            one = round(one, word(at));
            two = round(two, word(at + 8));
            three = round(three, word(at + 16));
            four = round(four, word(at + 24));
            at += 32;
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
    while at + 8 <= bytes.len() {
        hash ^= round(0, word(at));
        hash = hash.rotate_left(27).wrapping_mul(P1).wrapping_add(P4);
        at += 8;
    }
    if at + 4 <= bytes.len() {
        let tail = u32::from_le_bytes(bytes[at..at + 4].try_into().expect("four checksum bytes"));
        hash ^= u64::from(tail).wrapping_mul(P1);
        hash = hash.rotate_left(23).wrapping_mul(P2).wrapping_add(P3);
        at += 4;
    }
    while at < bytes.len() {
        hash ^= u64::from(bytes[at]).wrapping_mul(P5);
        hash = hash.rotate_left(11).wrapping_mul(P1);
        at += 1;
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
    /// The sort compares the first eight bytes as one integer before it compares the values, which
    /// settles almost every pair without touching the payload. Padding with zero on the right is
    /// order preserving for byte strings, because a shorter value differs from a longer one that
    /// starts the same way at a position where the shorter one has run out, and zero is below every
    /// byte that could be there. A pair the head cannot settle falls through to the bytes.
    ///
    /// The heads are kept rather than thrown away once the sort is over, because a reader searching
    /// this order wants exactly the same comparison and for exactly the same reason. Eight bytes an
    /// entry of file is what buys a binary search that reads no values at all in the ordinary case.
    fn ranked(&self) -> Vec<(u64, u32)> {
        let count = self.offsets.len() - 1;
        let mut ranked = (0..count)
            .map(|code| {
                let code = code as u32;
                (head(self.bytes(code).unwrap_or_default()), code)
            })
            .collect::<Vec<_>>();
        ranked.sort_unstable_by(|left, right| {
            left.0.cmp(&right.0).then_with(|| self.bytes(left.1).cmp(&self.bytes(right.1)))
        });
        ranked
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

/// Appends pages and commits a new directory for one table.
#[derive(Debug)]
pub struct Writer {
    file: File,
    table: Table,
    generation: u64,
    /// The first and the last source position in every stripe, in the order the stripes were
    /// written.
    order: Vec<((u64, u64), (u64, u64))>,
    next_order: u64,
    dictionaries: Vec<Option<GlobalDictionary>>,
    pending: Vec<PendingPart>,
}

#[derive(Debug)]
struct PendingPart {
    order: (u64, u64),
    rows: usize,
    pages: Vec<Vec<u8>>,
    codes: Vec<Option<Vec<u32>>>,
    zone: Zone,
}

/// Parts in one stripe.
///
/// Sixty four thousand rows is the smallest stripe that keeps the ClickBench directory in single
/// digit megabytes at a hundred million rows, and it puts a four byte column's page at a quarter of
/// a megabyte, which is the size a sequential read wants. Larger stripes buy a smaller directory
/// and cost a sparse fetch, which has to read a page index before it can reach one part.
const STRIPE_PARTS: usize = 64;

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
        let mut file =
            OpenOptions::new().write(true).read(true).create_new(true).open(path).map_err(io)?;
        let mut header = [0; HEADER as usize];
        header[..8].copy_from_slice(MAGIC);
        header[8..12].copy_from_slice(&FORMAT.to_le_bytes());
        file.write_all(&header).map_err(io)?;
        Ok(Self {
            file,
            dictionaries: fields
                .iter()
                .map(|field| (field.ty == LogicalType::Varchar).then(GlobalDictionary::new))
                .collect(),
            table: Table {
                name: name.into(),
                dictionaries: vec![None; fields.len()],
                fields,
                stripes: Vec::new(),
                rows: 0,
                frequencies: Vec::new(),
            },
            generation: 1,
            order: Vec::new(),
            next_order: 0,
            pending: Vec::with_capacity(STRIPE_PARTS),
        })
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
        if chunk.width() != self.table.fields.len() {
            return Err(invalid("chunk width differs from table schema"));
        }
        let mut pages = Vec::with_capacity(chunk.width());
        let mut codes = Vec::with_capacity(chunk.width());
        for (index, field) in self.table.fields.iter().enumerate() {
            let column = chunk.column(index)?;
            if column.logical_type() != &field.ty {
                return Err(invalid("chunk type differs from table schema"));
            }
            let (bytes, unique) = encode(column, self.dictionaries[index].as_mut())?;
            if bytes.len() > MAX_PAGE {
                return Err(invalid("column page exceeds the configured bound"));
            }
            pages.push(bytes);
            codes.push(unique);
        }
        self.table.rows = self
            .table
            .rows
            .checked_add(chunk.len())
            .ok_or_else(|| invalid("row count overflow"))?;
        if self.pending.last().is_some_and(|last| last.order > order) {
            self.flush_pending()?;
        }
        self.pending.push(PendingPart {
            order,
            rows: chunk.len(),
            pages,
            codes,
            zone: Zone::of(chunk),
        });
        if self.pending.len() == STRIPE_PARTS {
            self.flush_pending()?;
        }
        Ok(())
    }

    /// Writes the buffered parts as one stripe, each column's parts contiguous on disk.
    fn flush_pending(&mut self) -> Result<()> {
        if self.pending.is_empty() {
            return Ok(());
        }
        let width = self.table.fields.len();
        let parts = self.pending.len();
        let mut pages = Vec::with_capacity(width);
        let mut memberships = vec![None; width];
        let mut ranges = Vec::with_capacity(width);
        let mut index = Vec::with_capacity(width.saturating_mul(index_section(parts)?));
        for column in 0..width {
            let offset = self.file.stream_position().map_err(io)?;
            let section = index.len();
            let mut length = 0_usize;
            for pending in &self.pending {
                let bytes = &pending.pages[column];
                self.file.write_all(bytes).map_err(io)?;
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
            pages.push(Span {
                offset,
                length: u32::try_from(length).map_err(|_| invalid("page length overflow"))?,
            });
            ranges.push(merged_range(
                self.pending
                    .iter()
                    .map(|pending| pending.zone.column(column).cloned().unwrap_or_default()),
            ));
        }
        for (column, membership) in memberships.iter_mut().enumerate() {
            if self.pending.iter().all(|pending| pending.codes[column].is_none()) {
                continue;
            }
            let lists = self
                .pending
                .iter()
                .map(|pending| pending.codes[column].clone().unwrap_or_default())
                .collect::<Vec<_>>();
            let bytes = encode_membership(&merged_codes(lists));
            let offset = self.file.stream_position().map_err(io)?;
            self.file.write_all(&bytes).map_err(io)?;
            *membership = Some(Page {
                offset,
                length: u32::try_from(bytes.len())
                    .map_err(|_| invalid("membership page length overflow"))?,
                hash: checksum(&bytes),
            });
        }
        let offset = self.file.stream_position().map_err(io)?;
        self.file.write_all(&index).map_err(io)?;
        let index = Span {
            offset,
            length: u32::try_from(index.len())
                .map_err(|_| invalid("index page length overflow"))?,
        };
        let mut rows = 0_usize;
        let mut lengths = Vec::with_capacity(parts);
        let mut span = None;
        for pending in self.pending.drain(..) {
            rows = rows.checked_add(pending.rows).ok_or_else(|| invalid("row count overflow"))?;
            lengths
                .push(u32::try_from(pending.rows).map_err(|_| invalid("part row count overflow"))?);
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
            zone: Zone::from_ranges(ranges),
        });
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
        entries.sort_unstable_by(|left, right| {
            right.count.cmp(&left.count).then_with(|| frequency_order(left.value, right.value))
        });
        let omitted_max =
            entries.get(FREQUENCY_ENTRIES).map_or(decrements, |entry| decrements.max(entry.count));
        entries.truncate(FREQUENCY_ENTRIES);
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
    fn numeric_frequencies(&self) -> Result<Vec<Option<FrequencySummary>>> {
        let columns = self
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
        let width = columns.len().div_ceil(workers);
        let pieces = std::thread::scope(|scope| {
            columns
                .chunks(width)
                .map(|columns| {
                    scope.spawn(|| {
                        columns
                            .iter()
                            .map(|&column| Ok((column, self.numeric_frequency(column)?)))
                            .collect::<Result<Vec<_>>>()
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

    /// Commits the directory and syncs the file before publishing its header slot.
    ///
    /// # Errors
    ///
    /// If directory encoding, writing, or syncing fails.
    pub fn finish(mut self) -> Result<Table> {
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
            self.table.frequencies[index] = Some(code_frequency(&dictionary));
            let encoded = encode_global_dictionary(dictionary, &order)?;
            let offset = self.file.stream_position().map_err(io)?;
            self.file.write_all(&encoded.index).map_err(io)?;
            self.file.write_all(&encoded.ranks).map_err(io)?;
            self.file.write_all(&encoded.payload).map_err(io)?;
            let length = encoded
                .index
                .len()
                .checked_add(encoded.ranks.len())
                .and_then(|len| len.checked_add(encoded.payload.len()))
                .ok_or_else(|| invalid("dictionary page length overflow"))?;
            self.table.dictionaries[index] = Some(Page {
                offset,
                length: u32::try_from(length)
                    .map_err(|_| invalid("dictionary page length overflow"))?,
                hash: checksum(&encoded.index),
            });
        }
        let directory = encode_directory(&self.table)?;
        if directory.len() > MAX_DIRECTORY {
            return Err(invalid("directory exceeds the configured bound"));
        }
        let offset = self.file.stream_position().map_err(io)?;
        self.file.write_all(&directory).map_err(io)?;
        self.file.sync_all().map_err(io)?;
        let slot = Slot {
            offset,
            length: u32::try_from(directory.len())
                .map_err(|_| invalid("directory length overflow"))?,
            generation: self.generation,
            hash: checksum(&directory),
        };
        self.file.seek(SeekFrom::Start(16)).map_err(io)?;
        self.file.write_all(&slot.bytes()).map_err(io)?;
        self.file.sync_all().map_err(io)?;
        Ok(self.table)
    }
}

/// Reads committed native column pages without holding the table in memory.
#[derive(Debug, Clone)]
pub struct Reader {
    file: Arc<File>,
    table: Arc<Table>,
    dictionaries: Arc<Vec<OnceLock<Arc<Vector>>>>,
    /// Which stripe and which part of it every part of the table is, by table wide part number.
    places: Arc<Vec<Place>>,
    cache: Arc<Vec<Mutex<Cached>>>,
    /// How many whole stripe pages have been read, which is what the sharing above is judged on. A
    /// scan of a column should read each of its stripes once however many workers it has.
    pages: Arc<AtomicUsize>,
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
/// The second list is what keeps a scan from reading the same page once per worker. It is a list
/// and not a set because it holds at most one stripe per worker on the column and is walked far
/// less often than a hash of it would be built.
#[derive(Debug, Default)]
struct Cached {
    pages: Vec<CachedColumn>,
    loading: Vec<usize>,
}

/// Stripes of one column a reader keeps the bytes of.
///
/// This has to hold at least as many stripes as a column has workers straddling a stripe boundary
/// at once, or the workers evict each other's pages and read them again. Parts are handed out in
/// order so that is a small number. It multiplies by the page size, which is a quarter of a
/// megabyte for a four byte column, and by the number of columns a query touches.
const CACHED_STRIPES_PER_COLUMN: usize = 4;

type CrossingCache = OnceLock<Box<[OnceLock<Result<Vec<u8>>>]>>;

#[derive(Debug)]
struct NativeText {
    file: Arc<File>,
    offsets: Vec<u32>,
    /// How many entries the sorted order has, which is the value count.
    ranks: usize,
    /// Where the sorted order starts in the file. It is read a block at a time and only when
    /// something searches it, so a query that never compares this column against a literal never
    /// touches it at all.
    rank_at: u64,
    rank_hashes: Vec<u64>,
    rank_blocks: Vec<OnceLock<Result<Vec<u8>>>>,
    payload: u64,
    payload_len: usize,
    hashes: Vec<u64>,
    /// The payload, read and kept an extent at a time. See [`TEXT_PAYLOAD_EXTENT`].
    payload_extents: Vec<OnceLock<Result<Vec<u8>>>>,
    crossing: Vec<CrossingCache>,
}

const TEXT_PAYLOAD_BLOCK: usize = 64 * 1024;

/// How many blocks are read, allocated and waited on as one.
///
/// The block is what a checksum covers and it is written into the file, so it cannot move without
/// the format moving. What a reader does with it can. A scan of a string column ends up wanting
/// every block, because the codes a part holds are spread over the whole dictionary, and reading
/// them one at a time made a hundred million row `LIKE` spend more than half its time in the kernel
/// rather than in the predicate: a pread and a `Vec` per sixty four kilobytes, over a dictionary
/// that is more than a gigabyte, is twenty thousand of each. Under a poor man's profile of ClickBench
/// query 21, 58 percent of the samples were in a syscall, 18 percent were in `mprotect` with the
/// allocator growing the heap by sixty four kilobytes at a time, and 20 percent were threads parked
/// on a `OnceLock` somebody else was filling.
///
/// Eight blocks is half a megabyte, which is one read, one allocation the allocator takes straight
/// from `mmap` rather than off the heap, and one wait. The cost is paid by a query that wants a few
/// values rather than a column of them, which now reads half a megabyte to get at sixty four
/// kilobytes, and that is what picks the number. Over the queries that go each way, with the scan
/// being ClickBench query 21 and the few value read being query 34, which takes its answer out of
/// the frequency page and then looks ten codes up: four blocks is 1.318s and 0.075s, eight is 1.168s
/// and 0.084s, sixteen is 1.205s and 0.116s. The scan stops improving after eight and the lookup
/// keeps getting worse.
const TEXT_PAYLOAD_EXTENT: usize = 8;
const TEXT_CROSSING_BLOCK: usize = 1024;

/// How many entries of a dictionary's sorted order sit in one block that is read and checked as a
/// unit.
///
/// Five hundred and twelve entries is six kilobytes, which is a page and a half. A binary search
/// over half a million entries makes nineteen probes, and the first ten land in ten different
/// blocks while the last nine land in the one block that holds the answer, so the whole search
/// reads about sixty six kilobytes of a two megabyte order. A smaller block would save a little on
/// the early probes and cost a checksum list four times as long. A larger one would read more than
/// it uses on every probe.
const TEXT_RANK_BLOCK: usize = 512;

/// Bytes one entry of the sorted order takes: eight for the head and four for the code.
const RANK_ENTRY: usize = size_of::<u64>() + size_of::<u32>();

impl NativeText {
    fn payload_block(&self, block: usize) -> Result<Option<&[u8]>> {
        if block >= self.hashes.len() {
            return Ok(None);
        }
        let extent = block / TEXT_PAYLOAD_EXTENT;
        let Some(slot) = self.payload_extents.get(extent) else { return Ok(None) };
        let bytes = slot
            .get_or_init(|| {
                let start = extent
                    .checked_mul(TEXT_PAYLOAD_EXTENT * TEXT_PAYLOAD_BLOCK)
                    .ok_or_else(|| invalid("global dictionary block offset overflow"))?;
                let len = (TEXT_PAYLOAD_EXTENT * TEXT_PAYLOAD_BLOCK).min(
                    self.payload_len
                        .checked_sub(start)
                        .ok_or_else(|| invalid("global dictionary block starts past payload"))?,
                );
                let mut bytes = vec![0; len];
                read_at(&self.file, self.payload + start as u64, &mut bytes)?;
                // The checksums are per block and stay per block, because they are in the file. The
                // extent is only how much of the file one read and one allocation cover.
                for (within, piece) in bytes.chunks(TEXT_PAYLOAD_BLOCK).enumerate() {
                    if checksum(piece)
                        != *self
                            .hashes
                            .get(extent * TEXT_PAYLOAD_EXTENT + within)
                            .ok_or_else(|| invalid("global dictionary block has no checksum"))?
                    {
                        return Err(invalid("global dictionary payload checksum differs"));
                    }
                }
                Ok(bytes)
            })
            .as_ref()
            .map_err(Clone::clone)?;
        let within = (block % TEXT_PAYLOAD_EXTENT) * TEXT_PAYLOAD_BLOCK;
        let end = (within + TEXT_PAYLOAD_BLOCK).min(bytes.len());
        Ok(bytes.get(within..end))
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
                let first = rank / TEXT_RANK_BLOCK * TEXT_RANK_BLOCK;
                let len = TEXT_RANK_BLOCK.min(self.ranks - first) * RANK_ENTRY;
                let mut bytes = vec![0; len];
                read_at(&self.file, self.rank_at + (first * RANK_ENTRY) as u64, &mut bytes)?;
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
        let at = within * size_of::<u64>();
        let bytes = block
            .get(at..at + size_of::<u64>())
            .ok_or_else(|| invalid("global dictionary rank block is short of heads"))?;
        Ok(u64::from_le_bytes(bytes.try_into().expect("eight bytes")))
    }
}

impl TextSource for NativeText {
    fn len(&self) -> usize {
        self.offsets.len().saturating_sub(1)
    }

    fn bytes_at(&self, index: usize) -> Result<Option<&[u8]>> {
        let (Some(&start), Some(&end)) = (self.offsets.get(index), self.offsets.get(index + 1))
        else {
            return Ok(None);
        };
        if start == end {
            return Ok(Some(&[]));
        }
        let first = start as usize / TEXT_PAYLOAD_BLOCK;
        let last = (end as usize - 1) / TEXT_PAYLOAD_BLOCK;
        if first == last {
            let Some(block) = self.payload_block(first)? else { return Ok(None) };
            let within = start as usize % TEXT_PAYLOAD_BLOCK;
            return Ok(block.get(within..within + (end - start) as usize));
        }
        let Some(crossing) = self.crossing.get(index / TEXT_CROSSING_BLOCK) else {
            return Ok(None);
        };
        let block = crossing.get_or_init(|| {
            (0..TEXT_CROSSING_BLOCK).map(|_| OnceLock::new()).collect::<Vec<_>>().into_boxed_slice()
        });
        block[index % TEXT_CROSSING_BLOCK]
            .get_or_init(|| {
                let mut bytes = Vec::with_capacity((end - start) as usize);
                for part in first..=last {
                    let source = self
                        .payload_block(part)?
                        .ok_or_else(|| invalid("global dictionary block is missing"))?;
                    let from = if part == first { start as usize % TEXT_PAYLOAD_BLOCK } else { 0 };
                    let to = if part == last {
                        (end as usize - 1) % TEXT_PAYLOAD_BLOCK + 1
                    } else {
                        source.len()
                    };
                    bytes.extend_from_slice(source.get(from..to).ok_or_else(|| {
                        invalid("global dictionary value exceeds its payload block")
                    })?);
                }
                Ok(bytes)
            })
            .as_ref()
            .map(|bytes| Some(bytes.as_slice()))
            .map_err(Clone::clone)
    }

    fn bytes_len_at(&self, index: usize) -> Result<Option<usize>> {
        let (Some(&start), Some(&end)) = (self.offsets.get(index), self.offsets.get(index + 1))
        else {
            return Ok(None);
        };
        Ok(Some((end - start) as usize))
    }

    fn ranks(&self) -> Option<usize> {
        (self.ranks > 0).then_some(self.ranks)
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
        let heads = block.len() / RANK_ENTRY * size_of::<u64>();
        let at = heads + within * size_of::<u32>();
        let bytes = block
            .get(at..at + size_of::<u32>())
            .ok_or_else(|| invalid("global dictionary rank block is short of codes"))?;
        let code = u32::from_le_bytes(bytes.try_into().expect("four bytes"));
        if code as usize >= self.len() {
            return Err(invalid("global dictionary order names a code it does not have"));
        }
        Ok(code)
    }

    fn footprint(&self) -> usize {
        self.offsets.capacity() * size_of::<u32>()
            + self.rank_hashes.capacity() * size_of::<u64>()
            + self.rank_blocks.capacity() * size_of::<OnceLock<Result<Vec<u8>>>>()
            + self
                .rank_blocks
                .iter()
                .filter_map(OnceLock::get)
                .filter_map(|result| result.as_ref().ok())
                .map(Vec::capacity)
                .sum::<usize>()
            + self.payload_extents.capacity() * size_of::<OnceLock<Result<Vec<u8>>>>()
            + self.hashes.capacity() * size_of::<u64>()
            + self
                .payload_extents
                .iter()
                .filter_map(OnceLock::get)
                .filter_map(|result| result.as_ref().ok())
                .map(Vec::capacity)
                .sum::<usize>()
            + self.crossing.capacity() * size_of::<CrossingCache>()
            + self
                .crossing
                .iter()
                .filter_map(OnceLock::get)
                .map(|block| {
                    block.len() * size_of::<OnceLock<Result<Vec<u8>>>>()
                        + block
                            .iter()
                            .filter_map(OnceLock::get)
                            .filter_map(|result| result.as_ref().ok())
                            .map(Vec::capacity)
                            .sum::<usize>()
                })
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
        return Err(invalid("index page section checksum differs"));
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
fn remember(cached: &mut Cached, held: &CachedColumn) {
    match cached.pages.iter().position(|page| page.stripe == held.stripe) {
        // An index only read and a page read can both be in flight over the same stripe, and
        // letting the first land on top of the second would throw away a page somebody read.
        Some(found) => {
            if held.page.is_some() || cached.pages[found].page.is_none() {
                cached.pages[found] = held.clone();
            }
        }
        None => {
            if cached.pages.len() == CACHED_STRIPES_PER_COLUMN {
                cached.pages.remove(0);
            }
            cached.pages.push(held.clone());
        }
    }
}

impl Reader {
    /// Opens the highest valid directory slot.
    ///
    /// # Errors
    ///
    /// If the file has no valid committed directory or a directory pointer is out of bounds.
    pub fn open(path: impl AsRef<Path>) -> Result<Self> {
        let mut file = File::open(path).map_err(io)?;
        let size = file.metadata().map_err(io)?.len();
        if size < HEADER {
            return Err(invalid("file is shorter than its header"));
        }
        let mut header = [0; HEADER as usize];
        file.read_exact(&mut header).map_err(io)?;
        let version = u32::from_le_bytes([header[8], header[9], header[10], header[11]]);
        if &header[..8] != MAGIC || version != FORMAT {
            return Err(invalid("magic or major version is unsupported"));
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
            if checksum(&bytes) == slot.hash
                && selected
                    .as_ref()
                    .is_none_or(|(old, _): &(Slot, Vec<u8>)| old.generation < slot.generation)
            {
                selected = Some((slot, bytes));
            }
        }
        let (_, bytes) = selected.ok_or_else(|| invalid("no committed directory slot is valid"))?;
        let table = decode_directory(&bytes, size)?;
        let places = places(&table)?;
        let dictionaries = (0..table.fields.len()).map(|_| OnceLock::new()).collect();
        let cache =
            (0..table.fields.len()).map(|_| Mutex::new(Cached::default())).collect::<Vec<_>>();
        Ok(Self {
            file: Arc::new(file),
            table: Arc::new(table),
            dictionaries: Arc::new(dictionaries),
            places: Arc::new(places),
            cache: Arc::new(cache),
            pages: Arc::new(AtomicUsize::new(0)),
        })
    }

    /// How many parts the table has, which is how many chunks a scan of it reads.
    #[must_use]
    pub fn parts(&self) -> usize {
        self.places.len()
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
        let dictionary =
            if field.ty == LogicalType::Varchar { self.dictionary(column)? } else { None };
        let mut out = Vec::with_capacity(summary.entries.len());
        for entry in &summary.entries {
            let value = match entry.value {
                FrequencyValue::Null => Value::Null,
                FrequencyValue::Integer(value) => match field.ty {
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
        Ok(Some(out))
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
    /// `None` for a column the file has no dictionary for, which is every column that is not a
    /// string, and `None` for a column with a null in it. A sketch would answer the first
    /// approximately and SQL asked for the exact number. The second is the placeholder: a null row
    /// is written as the code for the empty string, so a nullable column's dictionary may hold an
    /// empty string that no row of it actually has, and nothing persisted today tells the two cases
    /// apart.
    ///
    /// # Errors
    ///
    /// If the column is outside the schema, or the dictionary page does not read.
    pub fn distinct_values(&self, column: usize) -> Result<Option<u64>> {
        if self.null_count(column)? > 0 {
            return Ok(None);
        }
        Ok(self.dictionary(column)?.map(|dictionary| dictionary.len() as u64))
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

    fn dictionary(&self, column: usize) -> Result<Option<Arc<Vector>>> {
        let Some(page) = self.table.dictionaries[column] else { return Ok(None) };
        if let Some(dictionary) = self.dictionaries[column].get() {
            return Ok(Some(Arc::clone(dictionary)));
        }
        let dictionary = Arc::new(open_global_dictionary(
            Arc::clone(&self.file),
            page,
            &self.table.fields[column].ty,
        )?);
        let _ = self.dictionaries[column].set(Arc::clone(&dictionary));
        Ok(Some(self.dictionaries[column].get().map_or(dictionary, Arc::clone)))
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
        let found = cached.pages.iter().find(|held| held.stripe == at).cloned();
        if let Some(found) = found.clone() {
            if !whole || found.page.is_some() {
                return Ok(found);
            }
        }
        if cached.loading.contains(&at) {
            drop(cached);
            if let Some(found) = found {
                return Ok(found);
            }
            let held = self.page_of(stripe, column, at, false)?;
            let mut cached = cache.lock().map_err(|_| invalid("column page cache is poisoned"))?;
            remember(&mut cached, &held);
            return Ok(held);
        }
        cached.loading.push(at);
        drop(cached);

        let read = self.page_of(stripe, column, at, whole);

        // The stripe leaves the loading list and its page enters the cache under one lock. Doing
        // them separately would leave a moment where another worker sees neither and reads the
        // page a second time, which is the whole thing this is here to stop.
        let mut cached = cache.lock().map_err(|_| invalid("column page cache is poisoned"))?;
        if let Some(position) = cached.loading.iter().position(|loading| *loading == at) {
            cached.loading.remove(position);
        }
        let held = read?;
        remember(&mut cached, &held);
        Ok(held)
    }

    /// Reads one stripe's index for a column, and its page when the caller wants all of it.
    fn page_of(
        &self,
        stripe: &Stripe,
        column: usize,
        at: usize,
        whole: bool,
    ) -> Result<CachedColumn> {
        let index = Arc::new(read_index(&self.file, stripe, column)?);
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
                return Err(invalid("column page checksum differs"));
            }
            let dictionary = self.dictionary(column)?;
            picked.push(decode(&field.ty, rows, bytes, dictionary)?);
        }
        Chunk::with_rows(picked, rows)
    }

    /// Whether persisted bounds prove that the stripe holding a part cannot match the predicates.
    ///
    /// The bounds are per stripe, so every part of a stripe gets the same answer. A scan that skips
    /// one part of a stripe this way skips all of them.
    #[must_use]
    pub fn skips(&self, part: usize, probes: &[Probe]) -> bool {
        self.stripe_of(part).is_ok_and(|stripe| stripe.zone.skips(probes))
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

#[cfg(not(unix))]
fn read_at(file: &File, offset: u64, bytes: &mut [u8]) -> Result<()> {
    let mut file = file.try_clone().map_err(io)?;
    file.seek(SeekFrom::Start(offset)).map_err(io)?;
    file.read_exact(bytes).map_err(io)
}

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
        _ => Err(Error::not_implemented(format!("native storage for {ty}"))),
    }
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
    entries.sort_unstable_by(|left, right| {
        right.count.cmp(&left.count).then_with(|| frequency_order(left.value, right.value))
    });
    let omitted_max = entries.get(FREQUENCY_ENTRIES).map_or(0, |entry| entry.count);
    entries.truncate(FREQUENCY_ENTRIES);
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
        out.push(type_tag(&field.ty)?);
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
        for (field, membership) in table.fields.iter().zip(&stripe.memberships) {
            if field.ty != LogicalType::Varchar {
                continue;
            }
            let page =
                membership.ok_or_else(|| invalid("string page has no code membership index"))?;
            put_u64(&mut out, page.offset);
            put_u32(&mut out, page.length);
            put_u64(&mut out, page.hash);
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
    Ok(out)
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
    fn bound(&mut self) -> Result<Option<Bound>> {
        Ok(match self.u8()? {
            0 => None,
            1 => Some(Bound::Int(i128::from_le_bytes(
                self.take(16)?.try_into().expect("sixteen bytes"),
            ))),
            2 => Some(Bound::Real(f64::from_le_bytes(
                self.take(8)?.try_into().expect("eight bytes"),
            ))),
            3 => {
                let length = self.u32()? as usize;
                Some(Bound::Bytes(self.take(length)?.to_vec()))
            }
            _ => return Err(invalid("bound tag differs")),
        })
    }
    fn text(&mut self) -> Result<String> {
        let len = self.u16()? as usize;
        String::from_utf8(self.take(len)?.to_vec()).map_err(|_| invalid("name is not UTF-8"))
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
        let ty = tag_type(cur.u8()?)?;
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
            if field.ty != LogicalType::Varchar {
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
        let mut ranges = Vec::with_capacity(width);
        for _ in 0..width {
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
            ranges.push(Range { low, high, nulls, exact, sum });
        }
        stripes.push(Stripe {
            rows: stripe_rows,
            parts,
            index,
            pages,
            memberships,
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
    if cur.at != bytes.len() {
        return Err(invalid("directory has trailing bytes"));
    }
    Ok(Table { name, fields, stripes, rows, dictionaries, frequencies })
}

fn put_bound(out: &mut Vec<u8>, bound: Option<&Bound>) -> Result<()> {
    match bound {
        None => out.push(0),
        Some(Bound::Int(value)) => {
            out.push(1);
            out.extend_from_slice(&value.to_le_bytes());
        }
        Some(Bound::Real(value)) => {
            out.push(2);
            out.extend_from_slice(&value.to_le_bytes());
        }
        Some(Bound::Bytes(value)) => {
            out.push(3);
            put_u32(out, u32::try_from(value.len()).map_err(|_| invalid("bound length overflow"))?);
            out.extend_from_slice(value);
        }
    }
    Ok(())
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
    let packed_vector = if dictionary.is_none() && global_codes.is_none() {
        Some(flat.bit_packed()?)
    } else {
        None
    };
    let packed = packed_vector.as_ref().and_then(Vector::packed_parts);
    out.push(if global_codes.is_some() {
        3
    } else if dictionary.is_some() {
        1
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
        (LogicalType::BigInt | LogicalType::Timestamp, Data::Int64(values)) => {
            for value in &**values {
                out.extend_from_slice(&value.to_le_bytes());
            }
        }
        (LogicalType::Boolean, Data::Bool(values)) => {
            for value in &**values {
                out.push(u8::from(*value));
            }
        }
        (LogicalType::Varchar, Data::Varlen(values)) => {
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
    payload: Vec<u8>,
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
    let payload_len = dictionary.payload.len();
    let blocks = payload_len.div_ceil(TEXT_PAYLOAD_BLOCK);
    let ranks = encode_ranks(order);
    let rank_blocks = values.div_ceil(TEXT_RANK_BLOCK);
    let mut index = Vec::with_capacity(12 + (values + 1) * 4 + (blocks + rank_blocks) * 8);
    put_u32(
        &mut index,
        u32::try_from(values).map_err(|_| invalid("global dictionary has too many values"))?,
    );
    put_u32(&mut index, TEXT_PAYLOAD_BLOCK as u32);
    put_u32(
        &mut index,
        u32::try_from(blocks).map_err(|_| invalid("global dictionary has too many blocks"))?,
    );
    for offset in dictionary.offsets {
        put_u32(&mut index, offset);
    }
    for block in dictionary.payload.chunks(TEXT_PAYLOAD_BLOCK) {
        put_u64(&mut index, checksum(block));
    }
    for block in ranks.chunks(TEXT_RANK_BLOCK * RANK_ENTRY) {
        put_u64(&mut index, checksum(block));
    }
    Ok(EncodedDictionary { index, ranks, payload: dictionary.payload })
}

/// The sorted order laid out the way a reader reads it, in blocks of [`TEXT_RANK_BLOCK`] entries.
///
/// Each block holds its heads first and then its codes, rather than pairing them, because a search
/// asks for a head at every probe and for a code about once a search. Keeping the heads together
/// means a probe touches eight bytes of a block rather than twelve spread over it, and the last few
/// probes of a search, which are the ones that land in the same block, touch the same cache line.
fn encode_ranks(order: &[(u64, u32)]) -> Vec<u8> {
    let mut out = Vec::with_capacity(order.len() * RANK_ENTRY);
    for block in order.chunks(TEXT_RANK_BLOCK) {
        for &(head, _) in block {
            put_u64(&mut out, head);
        }
        for &(_, code) in block {
            put_u32(&mut out, code);
        }
    }
    out
}

fn open_global_dictionary(file: Arc<File>, page: Page, ty: &LogicalType) -> Result<Vector> {
    if ty != &LogicalType::Varchar {
        return Err(invalid("global dictionary belongs to a non-string column"));
    }
    let mut header = [0; 12];
    read_at(&file, page.offset, &mut header)?;
    let count = u32::from_le_bytes(header[0..4].try_into().expect("four bytes")) as usize;
    let block_size = u32::from_le_bytes(header[4..8].try_into().expect("four bytes")) as usize;
    let blocks = u32::from_le_bytes(header[8..12].try_into().expect("four bytes")) as usize;
    if block_size != TEXT_PAYLOAD_BLOCK {
        return Err(invalid("global dictionary block width differs"));
    }
    let offset_len = (count + 1)
        .checked_mul(4)
        .ok_or_else(|| invalid("global dictionary offset count overflow"))?;
    // The sorted order is kept out of the index on purpose. The index is read and checksummed in
    // full the moment the column is first touched, and the order is two thirds the size of the
    // offsets, so putting it there would make every query that reads a string column pay for a
    // search that most of them never make.
    let ranks = count;
    let rank_blocks = ranks.div_ceil(TEXT_RANK_BLOCK);
    let rank_len =
        ranks.checked_mul(RANK_ENTRY).ok_or_else(|| invalid("global dictionary rank overflow"))?;
    let hash_len = blocks
        .checked_add(rank_blocks)
        .and_then(|count| count.checked_mul(8))
        .ok_or_else(|| invalid("global dictionary block count overflow"))?;
    let index_len = 12usize
        .checked_add(offset_len)
        .and_then(|len| len.checked_add(hash_len))
        .ok_or_else(|| invalid("global dictionary header overflow"))?;
    let body_len = index_len
        .checked_add(rank_len)
        .ok_or_else(|| invalid("global dictionary header overflow"))?;
    if body_len > page.length as usize {
        return Err(invalid("global dictionary offset index exceeds its page"));
    }
    let mut index = vec![0; index_len];
    index[..12].copy_from_slice(&header);
    read_at(&file, page.offset + 12, &mut index[12..])?;
    if checksum(&index) != page.hash {
        return Err(invalid("global dictionary index checksum differs"));
    }
    let offsets = index[12..12 + offset_len]
        .chunks_exact(4)
        .map(|part| u32::from_le_bytes(part.try_into().expect("four bytes")))
        .collect::<Vec<_>>();
    let mut hashes = index[12 + offset_len..]
        .chunks_exact(8)
        .map(|part| u64::from_le_bytes(part.try_into().expect("eight bytes")))
        .collect::<Vec<_>>();
    let rank_hashes = hashes.split_off(blocks);
    let payload_len = page.length as usize - body_len;
    if blocks != payload_len.div_ceil(TEXT_PAYLOAD_BLOCK) {
        return Err(invalid("global dictionary block count differs from its payload"));
    }
    if offsets.first() != Some(&0)
        || offsets.last().copied().map(|last| last as usize) != Some(payload_len)
        || offsets.windows(2).any(|pair| pair[0] > pair[1])
    {
        return Err(invalid("global dictionary offsets do not bound the payload"));
    }
    let payload_extents = (0..payload_len.div_ceil(TEXT_PAYLOAD_BLOCK * TEXT_PAYLOAD_EXTENT))
        .map(|_| OnceLock::new())
        .collect();
    let crossing = (0..count.div_ceil(TEXT_CROSSING_BLOCK)).map(|_| OnceLock::new()).collect();
    Vector::external_text(
        LogicalType::Varchar,
        Arc::new(NativeText {
            file,
            offsets,
            ranks,
            rank_at: page.offset + index_len as u64,
            rank_hashes,
            rank_blocks: (0..rank_blocks).map(|_| OnceLock::new()).collect(),
            payload: page.offset + body_len as u64,
            payload_len,
            hashes,
            payload_extents,
            crossing,
        }),
    )
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
        let mut strings = StringColumn::over(Buffer::from_vec(payload));
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
    if codec == 3 {
        let dictionary = global.ok_or_else(|| invalid("global code page has no dictionary"))?;
        let mut codes = Vec::with_capacity(rows);
        let mut highest = None;
        for _ in 0..rows {
            let code = cur.u32()?;
            highest = Some(highest.map_or(code, |old: u32| old.max(code)));
            codes.push(code);
        }
        if cur.at != bytes.len() {
            return Err(invalid("global code page has trailing bytes"));
        }
        return Ok(Vector::stable_dictionary_validated(codes, dictionary, highest)?
            .with_validity(validity));
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
        LogicalType::BigInt | LogicalType::Timestamp => {
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
        LogicalType::Boolean => {
            let values = cur.take(rows)?;
            if values.iter().any(|value| *value > 1) {
                return Err(invalid("boolean page has another value"));
            }
            Data::Bool(values.iter().map(|value| *value == 1).collect::<Vec<_>>().into())
        }
        LogicalType::Varchar => {
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
            let mut values = StringColumn::over(Buffer::from_vec(payload));
            for pair in offsets.windows(2) {
                values.push_in_place(pair[0] as usize, (pair[1] - pair[0]) as usize)?;
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

    use rudb_common::Value;
    use rudb_common::bounds::Op;

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
        let mut header = [0; 12];
        read_at(&reader.file, dictionary.offset, &mut header).expect("dictionary header");
        let count = u64::from(u32::from_le_bytes(header[0..4].try_into().expect("four bytes")));
        let blocks = u64::from(u32::from_le_bytes(header[8..12].try_into().expect("four bytes")));
        let rank_blocks = count.div_ceil(TEXT_RANK_BLOCK as u64);
        let index_len =
            12 + (count + 1) * 4 + (blocks + rank_blocks) * 8 + count * RANK_ENTRY as u64;
        let mut file = OpenOptions::new().write(true).open(&path).expect("open dictionary page");
        file.seek(SeekFrom::Start(dictionary.offset + index_len))
            .expect("inside dictionary payload");
        file.write_all(&[255]).expect("damage dictionary payload");

        let chunk = reader.read(0, &[1]).expect("code page and dictionary index remain valid");
        let error =
            chunk.validate_external().expect_err("payload corruption must reach the caller");
        assert!(error.message().contains("payload checksum differs"), "{error}");
        fs::remove_file(path).expect("remove scratch file");
    }

    /// A payload that spans more than one extent still reads and checks every block of it.
    ///
    /// The test above has a dictionary of three values, so it says nothing about the grouping a
    /// reader does over the blocks the checksums are written for. This one is over a megabyte,
    /// which is more than one extent, and it reads a value out of the first extent and a value out
    /// of the last and then damages the last and asks for it again.
    #[test]
    fn a_dictionary_over_one_extent_checks_every_block_of_it() {
        let path = path("dictionary-extents");
        let value = |row: usize| format!("{row:07} a value long enough to be worth a payload block");
        let parts = 30;
        let per_part = 1000;
        let mut writer =
            Writer::create(&path, "items", vec![Field::required("text", LogicalType::Varchar)])
                .expect("new file");
        for part in 0..parts {
            let values = (0..per_part)
                .map(|row| Value::Varchar(value(part * per_part + row)))
                .collect::<Vec<_>>();
            let chunk =
                Chunk::new(vec![Vector::from_values(LogicalType::Varchar, &values)
                    .expect("strings")])
                .expect("matching rows");
            writer.append(&chunk).expect("a part");
        }
        writer.finish().expect("commit");

        let reader = Reader::open(&path).expect("reopen from disk");
        let dictionary = reader.table.dictionaries[0].expect("string dictionary page");
        assert!(
            dictionary.length as usize > TEXT_PAYLOAD_BLOCK * TEXT_PAYLOAD_EXTENT,
            "the dictionary has to be over one extent for this to be testing anything"
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
        let mut header = [0; 12];
        read_at(&reader.file, page.offset, &mut header).expect("dictionary header");
        let count = u64::from(u32::from_le_bytes(header[0..4].try_into().expect("four bytes")));
        let blocks = u64::from(u32::from_le_bytes(header[8..12].try_into().expect("four bytes")));
        let rank_blocks = count.div_ceil(TEXT_RANK_BLOCK as u64);
        let index_len = 12 + (count + 1) * 4 + (blocks + rank_blocks) * 8;
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
            frequencies: vec![None],
        };
        let directory = encode_directory(&table).expect("directory");
        let file_size = dictionary.offset + u64::from(dictionary.length) + 1;

        let decoded = decode_directory(&directory, file_size).expect("large lazy dictionary");
        assert_eq!(decoded.dictionaries[0].expect("dictionary").length, dictionary.length);
    }
}
