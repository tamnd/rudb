//! Rudb's single-file columnar snapshot format.
//!
//! A committed directory names independently readable column pages. The first version handles
//! scalar columns and one table; the file header already has two generation slots so an unfinished
//! replacement directory cannot hide the last complete one.

#![forbid(unsafe_code)]

use std::cmp::Ordering;
use std::collections::HashMap;
use std::fs::{File, OpenOptions};
use std::io::{Read, Seek, SeekFrom, Write};
use std::mem::size_of;
use std::path::Path;
use std::sync::Mutex;
use std::sync::{Arc, OnceLock};

use rudb_common::bounds::Bound;
use rudb_common::{Error, Field, LogicalType, Result, Value};
use rudb_storage::{Probe, Range, Zone};
use rudb_vector::string::StringColumn;
use rudb_vector::validity::Validity;
use rudb_vector::{Buffer, Chunk, Data, TextSource, Vector};

const MAGIC: &[u8; 8] = b"RUDBNV7\0";
const DIRECTORY: &[u8; 8] = b"RUDBDIR7";
const HEADER: u64 = 80;
const SLOT_BYTES: usize = 28;
const MAX_PAGE: usize = 256 * 1024 * 1024;
const MAX_DIRECTORY: usize = 128 * 1024 * 1024;
const FREQUENCIES: &[u8; 8] = b"RUDBFQ1\0";
const FREQUENCY_CANDIDATES: usize = 32_768;
const FREQUENCY_ENTRIES: usize = 512;
const FREQUENCY_BUILD_RANK: usize = 10;

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
}

/// One independently readable stripe of a table.
#[derive(Debug, Clone)]
pub struct Stripe {
    rows: usize,
    pages: Vec<Page>,
    zone: Zone,
}

impl Stripe {
    /// Number of rows in this stripe.
    #[must_use]
    pub fn rows(&self) -> usize {
        self.rows
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
    order: Vec<(u64, u64)>,
    next_order: u64,
    dictionaries: Vec<Option<GlobalDictionary>>,
    pending: Vec<PendingStripe>,
}

#[derive(Debug)]
struct PendingStripe {
    order: (u64, u64),
    rows: usize,
    pages: Vec<Vec<u8>>,
    zone: Zone,
}

const EXTENT_STRIPES: usize = 32;

impl Writer {
    /// Creates a new v7 file and its first table.
    ///
    /// # Errors
    ///
    /// If the file exists, a field has no v7 scalar encoding, or the path cannot be written.
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
        header[8..12].copy_from_slice(&7_u32.to_le_bytes());
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
            pending: Vec::with_capacity(EXTENT_STRIPES),
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
    /// Their directory entries are sorted by this key at commit, so a scan still observes source
    /// order without holding the page bytes until earlier work finishes.
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
        for (index, field) in self.table.fields.iter().enumerate() {
            let column = chunk.column(index)?;
            if column.logical_type() != &field.ty {
                return Err(invalid("chunk type differs from table schema"));
            }
            let bytes = encode(column, self.dictionaries[index].as_mut())?;
            if bytes.len() > MAX_PAGE {
                return Err(invalid("column page exceeds the configured bound"));
            }
            pages.push(bytes);
        }
        self.table.rows = self
            .table
            .rows
            .checked_add(chunk.len())
            .ok_or_else(|| invalid("row count overflow"))?;
        self.pending.push(PendingStripe { order, rows: chunk.len(), pages, zone: Zone::of(chunk) });
        if self.pending.len() == EXTENT_STRIPES {
            self.flush_pending()?;
        }
        Ok(())
    }

    /// Writes one bounded group of stripes with each column contiguous on disk.
    fn flush_pending(&mut self) -> Result<()> {
        if self.pending.is_empty() {
            return Ok(());
        }
        let width = self.table.fields.len();
        let mut pages = vec![Vec::with_capacity(width); self.pending.len()];
        for column in 0..width {
            for (stripe, pending) in self.pending.iter().enumerate() {
                let bytes = &pending.pages[column];
                let offset = self.file.stream_position().map_err(io)?;
                self.file.write_all(bytes).map_err(io)?;
                pages[stripe].push(Page {
                    offset,
                    length: u32::try_from(bytes.len())
                        .map_err(|_| invalid("page length overflow"))?,
                    hash: checksum(bytes),
                });
            }
        }
        for (pending, pages) in self.pending.drain(..).zip(pages) {
            self.table.stripes.push(Stripe { rows: pending.rows, pages, zone: pending.zone });
            self.order.push(pending.order);
        }
        Ok(())
    }

    /// Finds exact heavy hitters without keeping a hash table for every numeric column while the
    /// load is live. The pages are already in the target file, so one column at a time uses a
    /// bounded Misra-Gries candidate table and then recounts only those candidates.
    fn numeric_frequency(&self, column: usize) -> Result<Option<FrequencySummary>> {
        let ty = &self.table.fields[column].ty;
        if !matches!(
            ty,
            LogicalType::SmallInt
                | LogicalType::Integer
                | LogicalType::BigInt
                | LogicalType::Date
                | LogicalType::Timestamp
        ) {
            return Ok(None);
        }
        let mut candidates: HashMap<FrequencyValue, u32> = HashMap::new();
        let mut decrements = 0_u64;
        self.visit_numeric(column, |value| {
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
        let exact = if decrements == 0 {
            candidates
                .into_iter()
                .map(|(value, count)| (value, u64::from(count)))
                .collect::<HashMap<_, _>>()
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
            self.visit_numeric(column, |value| {
                if let Some(count) = exact.get_mut(&value) {
                    *count = count.saturating_add(1);
                }
            })?;
            exact
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
        Ok(Some(FrequencySummary { entries, omitted_max }))
    }

    fn visit_numeric(&self, column: usize, mut visit: impl FnMut(FrequencyValue)) -> Result<()> {
        let ty = &self.table.fields[column].ty;
        for stripe in &self.table.stripes {
            let page = stripe.pages[column];
            let mut bytes = vec![0; page.length as usize];
            read_at(&self.file, page.offset, &mut bytes)?;
            if checksum(&bytes) != page.hash {
                return Err(invalid("column page checksum differs while building frequencies"));
            }
            let vector = decode(ty, stripe.rows, &bytes, None)?;
            for row in 0..stripe.rows {
                let value = if vector.is_null_at(row) {
                    FrequencyValue::Null
                } else {
                    FrequencyValue::Integer(vector.signed_at(row).ok_or_else(|| {
                        invalid("numeric frequency page did not contain a signed value")
                    })?)
                };
                visit(value);
            }
        }
        Ok(())
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
        stripes.sort_by_key(|(order, _)| *order);
        self.table.stripes = stripes.into_iter().map(|(_, stripe)| stripe).collect();
        self.table.frequencies = (0..self.table.fields.len())
            .map(|column| self.numeric_frequency(column))
            .collect::<Result<Vec<_>>>()?;
        for (index, dictionary) in self.dictionaries.into_iter().enumerate() {
            let Some(dictionary) = dictionary else { continue };
            self.table.frequencies[index] = Some(code_frequency(&dictionary));
            let encoded = encode_global_dictionary(dictionary)?;
            let offset = self.file.stream_position().map_err(io)?;
            self.file.write_all(&encoded.index).map_err(io)?;
            self.file.write_all(&encoded.payload).map_err(io)?;
            let length = encoded
                .index
                .len()
                .checked_add(encoded.payload.len())
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
    extents: Arc<Vec<Vec<ExtentPart>>>,
    extent_cache: Arc<Vec<Mutex<Vec<CachedExtent>>>>,
}

#[derive(Debug, Clone, Copy, Default)]
struct ExtentPart {
    offset: u64,
    length: usize,
    page_start: usize,
}

#[derive(Debug)]
struct CachedExtent {
    offset: u64,
    bytes: Arc<Vec<u8>>,
}

const CACHED_EXTENTS_PER_COLUMN: usize = 8;

type CrossingCache = OnceLock<Box<[OnceLock<Result<Vec<u8>>>]>>;

#[derive(Debug)]
struct NativeText {
    file: Arc<File>,
    offsets: Vec<u32>,
    payload: u64,
    payload_len: usize,
    hashes: Vec<u64>,
    payload_blocks: Vec<OnceLock<Result<Vec<u8>>>>,
    crossing: Vec<CrossingCache>,
}

const TEXT_PAYLOAD_BLOCK: usize = 64 * 1024;
const TEXT_CROSSING_BLOCK: usize = 1024;

impl NativeText {
    fn payload_block(&self, block: usize) -> Result<Option<&[u8]>> {
        let Some(slot) = self.payload_blocks.get(block) else { return Ok(None) };
        slot.get_or_init(|| {
            let start = block
                .checked_mul(TEXT_PAYLOAD_BLOCK)
                .ok_or_else(|| invalid("global dictionary block offset overflow"))?;
            let len = TEXT_PAYLOAD_BLOCK.min(
                self.payload_len
                    .checked_sub(start)
                    .ok_or_else(|| invalid("global dictionary block starts past payload"))?,
            );
            let mut bytes = vec![0; len];
            read_at(&self.file, self.payload + start as u64, &mut bytes)?;
            if checksum(&bytes)
                != *self
                    .hashes
                    .get(block)
                    .ok_or_else(|| invalid("global dictionary block has no checksum"))?
            {
                return Err(invalid("global dictionary payload checksum differs"));
            }
            Ok(bytes)
        })
        .as_ref()
        .map(|bytes| Some(bytes.as_slice()))
        .map_err(Clone::clone)
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

    fn footprint(&self) -> usize {
        self.offsets.capacity() * size_of::<u32>()
            + self.payload_blocks.capacity() * size_of::<OnceLock<Result<Vec<u8>>>>()
            + self.hashes.capacity() * size_of::<u64>()
            + self
                .payload_blocks
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

/// Maps each logical page to the bounded contiguous read that contains it.
fn extent_parts(table: &Table) -> Result<Vec<Vec<ExtentPart>>> {
    let mut refs = Vec::with_capacity(table.stripes.len().saturating_mul(table.fields.len()));
    for (stripe, entry) in table.stripes.iter().enumerate() {
        for (column, page) in entry.pages.iter().enumerate() {
            refs.push((page.offset, column, stripe, page.length as usize));
        }
    }
    refs.sort_unstable_by_key(|entry| entry.0);
    let mut parts = vec![vec![ExtentPart::default(); table.fields.len()]; table.stripes.len()];
    let mut first = 0;
    while first < refs.len() {
        let (offset, column, _, first_len) = refs[first];
        let mut end = offset
            .checked_add(first_len as u64)
            .ok_or_else(|| invalid("column extent range overflow"))?;
        let mut last = first + 1;
        while last < refs.len()
            && last - first < EXTENT_STRIPES
            && refs[last].1 == column
            && refs[last].0 == end
        {
            end = end
                .checked_add(refs[last].3 as u64)
                .ok_or_else(|| invalid("column extent range overflow"))?;
            last += 1;
        }
        let length = usize::try_from(end - offset)
            .map_err(|_| invalid("column extent length exceeds this platform"))?;
        for &(_, _, stripe, _) in &refs[first..last] {
            let page = table.stripes[stripe].pages[column];
            let page_start = usize::try_from(page.offset - offset)
                .map_err(|_| invalid("column page offset exceeds this platform"))?;
            parts[stripe][column] = ExtentPart { offset, length, page_start };
        }
        first = last;
    }
    Ok(parts)
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
        if &header[..8] != MAGIC || header[8..12] != 7_u32.to_le_bytes() {
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
        let extents = extent_parts(&table)?;
        let dictionaries = (0..table.fields.len()).map(|_| OnceLock::new()).collect();
        let extent_cache = (0..table.fields.len())
            .map(|_| Mutex::new(Vec::with_capacity(CACHED_EXTENTS_PER_COLUMN)))
            .collect::<Vec<_>>();
        Ok(Self {
            file: Arc::new(file),
            table: Arc::new(table),
            dictionaries: Arc::new(dictionaries),
            extents: Arc::new(extents),
            extent_cache: Arc::new(extent_cache),
        })
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

    /// Reads only the named columns from one stripe.
    ///
    /// # Errors
    ///
    /// If a stripe, column, page, or checksum is invalid.
    pub fn read(&self, stripe: usize, columns: &[usize]) -> Result<Chunk> {
        self.read_impl(stripe, columns, true)
    }

    /// Reads named columns from one stripe without prefetching adjacent stripe pages.
    ///
    /// This is intended for sparse row fetches after a selective TopN or filter. Sequential scans
    /// should use [`Self::read`] so adjacent pages share one extent read.
    ///
    /// # Errors
    ///
    /// If a stripe, column, page, or checksum is invalid.
    pub fn read_sparse(&self, stripe: usize, columns: &[usize]) -> Result<Chunk> {
        self.read_impl(stripe, columns, false)
    }

    fn read_impl(&self, stripe: usize, columns: &[usize], prefetch: bool) -> Result<Chunk> {
        let stripe_index = stripe;
        let stripe =
            self.table.stripes.get(stripe).ok_or_else(|| invalid("stripe index out of range"))?;
        let mut picked = Vec::with_capacity(columns.len());
        for &column in columns {
            let field = self
                .table
                .fields
                .get(column)
                .ok_or_else(|| invalid("column index out of range"))?;
            let page = stripe.pages.get(column).ok_or_else(|| invalid("stripe page is missing"))?;
            let part = self
                .extents
                .get(stripe_index)
                .and_then(|parts| parts.get(column))
                .ok_or_else(|| invalid("column extent is missing"))?;
            let bytes = if !prefetch || part.length == page.length as usize {
                let mut bytes = vec![0; page.length as usize];
                read_at(&self.file, page.offset, &mut bytes)?;
                Arc::new(bytes)
            } else {
                let cached = self.extent_cache[column]
                    .lock()
                    .map_err(|_| invalid("column extent cache is poisoned"))?
                    .iter()
                    .find(|cached| cached.offset == part.offset)
                    .map(|cached| Arc::clone(&cached.bytes));
                if let Some(bytes) = cached {
                    bytes
                } else {
                    let mut bytes = vec![0; part.length];
                    read_at(&self.file, part.offset, &mut bytes)?;
                    let bytes = Arc::new(bytes);
                    let mut cache = self.extent_cache[column]
                        .lock()
                        .map_err(|_| invalid("column extent cache is poisoned"))?;
                    if let Some(cached) = cache.iter().find(|cached| cached.offset == part.offset) {
                        Arc::clone(&cached.bytes)
                    } else {
                        if cache.len() == CACHED_EXTENTS_PER_COLUMN {
                            cache.remove(0);
                        }
                        cache.push(CachedExtent { offset: part.offset, bytes: Arc::clone(&bytes) });
                        bytes
                    }
                }
            };
            let page_start = if prefetch { part.page_start } else { 0 };
            let end = page_start
                .checked_add(page.length as usize)
                .ok_or_else(|| invalid("column page range overflow"))?;
            let page_bytes = bytes
                .get(page_start..end)
                .ok_or_else(|| invalid("column page exceeds its extent"))?;
            if checksum(page_bytes) != page.hash {
                return Err(invalid("column page checksum differs"));
            }
            let dictionary = self.dictionary(column)?;
            picked.push(decode(&field.ty, stripe.rows, page_bytes, dictionary)?);
        }
        Chunk::with_rows(picked, stripe.rows)
    }

    /// Whether persisted bounds prove that a stripe cannot match the predicates.
    #[must_use]
    pub fn skips(&self, stripe: usize, probes: &[Probe]) -> bool {
        self.table.stripes.get(stripe).is_some_and(|stripe| stripe.zone.skips(probes))
    }
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
    FrequencySummary { entries, omitted_max }
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
            u32::try_from(stripe.rows).map_err(|_| invalid("stripe row count overflow"))?,
        );
        for page in &stripe.pages {
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
        let stripe_rows = cur.u32()? as usize;
        if stripe_rows == 0 {
            return Err(invalid("empty stripe"));
        }
        total =
            total.checked_add(stripe_rows).ok_or_else(|| invalid("stripe row count overflow"))?;
        let mut pages = Vec::with_capacity(width);
        for _ in 0..width {
            let offset = cur.u64()?;
            let length = cur.u32()?;
            let hash = cur.u64()?;
            let end = offset
                .checked_add(u64::from(length))
                .ok_or_else(|| invalid("page offset overflow"))?;
            if offset < HEADER || end > size || length as usize > MAX_PAGE {
                return Err(invalid("page range is outside the file"));
            }
            pages.push(Page { offset, length, hash });
        }
        let mut ranges = Vec::with_capacity(width);
        for _ in 0..width {
            let low = cur.bound()?;
            let high = cur.bound()?;
            let nulls = cur.u32()? as usize;
            if nulls > stripe_rows {
                return Err(invalid("null count exceeds stripe rows"));
            }
            ranges.push(Range { low, high, nulls });
        }
        stripes.push(Stripe { rows: stripe_rows, pages, zone: Zone::from_ranges(ranges) });
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
                                    LogicalType::SmallInt
                                        | LogicalType::Integer
                                        | LogicalType::BigInt
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
                    Some(FrequencySummary { entries, omitted_max })
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

fn encode(vector: &Vector, global: Option<&mut GlobalDictionary>) -> Result<Vec<u8>> {
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
        return Ok(out);
    }
    if let Some(dictionary) = dictionary {
        out.extend_from_slice(&dictionary);
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
        (LogicalType::SmallInt, Data::Int16(values)) => {
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
    Ok(out)
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
    payload: Vec<u8>,
}

fn encode_global_dictionary(dictionary: GlobalDictionary) -> Result<EncodedDictionary> {
    let values = dictionary.offsets.len() - 1;
    let payload_len = dictionary.payload.len();
    let blocks = payload_len.div_ceil(TEXT_PAYLOAD_BLOCK);
    let mut index = Vec::with_capacity(12 + (values + 1) * 4 + blocks * 8);
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
    Ok(EncodedDictionary { index, payload: dictionary.payload })
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
    let hash_len =
        blocks.checked_mul(8).ok_or_else(|| invalid("global dictionary block count overflow"))?;
    let index_len = 12usize
        .checked_add(offset_len)
        .and_then(|len| len.checked_add(hash_len))
        .ok_or_else(|| invalid("global dictionary header overflow"))?;
    if index_len > page.length as usize {
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
    let hashes = index[12 + offset_len..]
        .chunks_exact(8)
        .map(|part| u64::from_le_bytes(part.try_into().expect("eight bytes")))
        .collect::<Vec<_>>();
    let payload_len = page.length as usize - index_len;
    if blocks != payload_len.div_ceil(TEXT_PAYLOAD_BLOCK) {
        return Err(invalid("global dictionary block count differs from its payload"));
    }
    if offsets.first() != Some(&0)
        || offsets.last().copied().map(|last| last as usize) != Some(payload_len)
        || offsets.windows(2).any(|pair| pair[0] > pair[1])
    {
        return Err(invalid("global dictionary offsets do not bound the payload"));
    }
    let payload_blocks =
        (0..payload_len.div_ceil(TEXT_PAYLOAD_BLOCK)).map(|_| OnceLock::new()).collect();
    let crossing = (0..count.div_ceil(TEXT_CROSSING_BLOCK)).map(|_| OnceLock::new()).collect();
    Vector::external_text(
        LogicalType::Varchar,
        Arc::new(NativeText {
            file,
            offsets,
            payload: page.offset + index_len as u64,
            payload_len,
            hashes,
            payload_blocks,
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
        writer.append(&sample()).expect("first stripe");
        writer.append(&sample()).expect("second stripe");
        writer.finish().expect("commit");
        let reader = Reader::open(&path).expect("reopen from disk");
        assert_eq!(reader.table().rows(), 6);
        assert_eq!(reader.table().stripes().len(), 2);
        let text = reader.read(1, &[1]).expect("only text page");
        assert_eq!(text.width(), 1);
        assert_eq!(text.value_at(1, 0), Value::Null);
        assert_eq!(text.value_at(2, 0), Value::Varchar("long text after a slash".into()));
        let sparse = reader.read_sparse(1, &[1]).expect("one page without extent prefetch");
        assert_eq!(sparse.width(), 1);
        assert_eq!(sparse.value_at(1, 0), Value::Null);
        assert_eq!(sparse.value_at(2, 0), Value::Varchar("long text after a slash".into()));
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
        let index_len = 12_u64 + 4 * 4 + 8;
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
