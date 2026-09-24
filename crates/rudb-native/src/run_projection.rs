//! A row-preserving run codec for a sorted, covering integer projection.
//!
//! A page stores each sorted order value once with its run length. Raw runs keep every covered
//! code; uniform runs keep one code and the row count already in the header. Both layouts retain
//! duplicate `(order, covered)` rows. Pages end between order values so query workers can count
//! exact pairs independently.

use std::collections::{HashMap, HashSet};
use std::path::Path;

use rudb_common::Result;

use crate::projection::{eligible, id, integers};
use crate::{Catalog, Reader, attach, invalid, section};

const MAGIC: &[u8; 8] = b"RUDBRP1\0";
const LEGACY_PAGE_BYTES: usize = 1 << 19;
pub(crate) const RLE_PAGE_BYTES: usize = 1 << 16;
pub(crate) const RLE_PAGES: u32 = 1;
const FIXED_HEADER: usize = 8 + 2 + 2 + 8 + 2 + 4 + 1;
const PAGE_HEADER: usize = 4 + 4 + 8 + 8;
const RUN_HEADER: usize = 8 + 4;

/// Attach a row-preserving run projection to an existing native table.
///
/// The builder retains every source row's covered value and multiplicity. Consecutive equal
/// order values share their eight-byte order value, and uniform covered runs use lossless
/// run-length encoding. The caller must include this explicit build in indexed-load measurements.
/// An append makes
/// the section stale until it is rebuilt.
///
/// # Errors
///
/// If the table or columns are missing, nullable, unsupported, or the file cannot be updated.
/// This first page format also rejects one order-value run too large to fit in a single page.
pub fn build_run_projection(
    path: impl AsRef<Path>,
    table: &str,
    order_column: &str,
    covered_column: &str,
) -> Result<()> {
    let path = path.as_ref();
    let catalog = Catalog::open(path)?;
    let reader = catalog.table(table)?;
    let fields = reader.table().fields();
    let order = fields
        .iter()
        .position(|field| field.name.eq_ignore_ascii_case(order_column))
        .ok_or_else(|| invalid("run projection order column is missing"))?;
    let covered = fields
        .iter()
        .position(|field| field.name.eq_ignore_ascii_case(covered_column))
        .ok_or_else(|| invalid("run projection covered column is missing"))?;
    if order == covered
        || !fields[order].not_null
        || !fields[covered].not_null
        || !eligible(&fields[order].ty, &fields[covered].ty)
    {
        return Err(invalid("run projection needs two supported, non-null integer columns"));
    }
    let mut rows = Vec::<(i64, i32)>::with_capacity(reader.table().rows());
    let mut values = HashSet::<i32>::new();
    let (mut users, mut groups) = (Vec::new(), Vec::new());
    for part in 0..reader.parts() {
        let chunk = reader.read(part, &[order, covered])?;
        integers(chunk.column(0)?, &mut users)?;
        integers(chunk.column(1)?, &mut groups)?;
        for (&order_value, &group) in users.iter().zip(&groups) {
            let covered_value = i32::try_from(group)
                .map_err(|_| invalid("projection covered value exceeds INTEGER"))?;
            rows.push((order_value, covered_value));
            values.insert(covered_value);
        }
    }
    if rows.len() != reader.table().rows() {
        return Err(invalid("run projection row count differs from its table"));
    }
    let mut dictionary = values.into_iter().collect::<Vec<_>>();
    dictionary.sort_unstable();
    let dictionary_len = u16::try_from(dictionary.len())
        .map_err(|_| invalid("run projection covered column exceeds 65535 values"))?;
    let code_bytes = if dictionary.len() <= 256 { 1 } else { 2 };
    let codes = dictionary
        .iter()
        .enumerate()
        .map(|(at, &value)| (value, at as u16))
        .collect::<HashMap<_, _>>();
    rows.sort_unstable_by_key(|&(order_value, _)| order_value);
    let order_index =
        u16::try_from(order).map_err(|_| invalid("run projection column index overflow"))?;
    let covered_index =
        u16::try_from(covered).map_err(|_| invalid("run projection column index overflow"))?;
    let header = FIXED_HEADER + dictionary.len() * 4;
    if header + PAGE_HEADER >= LEGACY_PAGE_BYTES {
        return Err(invalid("run projection dictionary does not fit in its first page"));
    }
    let longest_run =
        rows.chunk_by(|left, right| left.0 == right.0).map(|run| run.len()).max().unwrap_or(0);
    let raw_run_bytes =
        longest_run.checked_mul(code_bytes).and_then(|bytes| bytes.checked_add(RUN_HEADER + 1));
    let rle_fits = header + PAGE_HEADER < RLE_PAGE_BYTES
        && raw_run_bytes.is_some_and(|bytes| bytes <= RLE_PAGE_BYTES - PAGE_HEADER - header);
    let flags = if rle_fits { RLE_PAGES } else { 0 };
    let page_bytes = if rle_fits { RLE_PAGE_BYTES } else { LEGACY_PAGE_BYTES };
    let mut bytes = vec![0_u8; page_bytes];
    bytes[..8].copy_from_slice(MAGIC);
    bytes[8..10].copy_from_slice(&order_index.to_le_bytes());
    bytes[10..12].copy_from_slice(&covered_index.to_le_bytes());
    bytes[12..20].copy_from_slice(&(rows.len() as u64).to_le_bytes());
    bytes[20..22].copy_from_slice(&dictionary_len.to_le_bytes());
    bytes[26] = code_bytes as u8;
    for (at, value) in dictionary.iter().enumerate() {
        let offset = FIXED_HEADER + at * 4;
        bytes[offset..offset + 4].copy_from_slice(&value.to_le_bytes());
    }
    let mut page = 0_usize;
    let mut cursor = header + PAGE_HEADER;
    let mut page_rows = 0_u32;
    let mut page_first = None;
    let mut page_last = None;
    let mut at = 0;
    while at < rows.len() {
        let user = rows[at].0;
        let mut end = at + 1;
        while end < rows.len() && rows[end].0 == user {
            end += 1;
        }
        let count = u32::try_from(end - at)
            .map_err(|_| invalid("run projection user run exceeds its page count"))?;
        let uniform = flags == RLE_PAGES
            && rows[at..end].iter().all(|&(_, covered_value)| covered_value == rows[at].1);
        let payload_bytes = if uniform { code_bytes } else { (end - at) * code_bytes };
        let run_bytes = RUN_HEADER + usize::from(flags == RLE_PAGES) + payload_bytes;
        if run_bytes > page_bytes - PAGE_HEADER {
            return Err(invalid("run projection user run exceeds a page"));
        }
        if cursor + run_bytes > (page + 1) * page_bytes {
            finish_page(&mut bytes, page, header, cursor, page_rows, page_first, page_last)?;
            page += 1;
            bytes.resize((page + 1) * page_bytes, 0);
            cursor = page * page_bytes + PAGE_HEADER;
            page_rows = 0;
            page_first = None;
        }
        if cursor + run_bytes > (page + 1) * page_bytes {
            return Err(invalid("run projection user run exceeds the first page"));
        }
        page_first.get_or_insert(user);
        page_last = Some(user);
        bytes[cursor..cursor + 8].copy_from_slice(&user.to_le_bytes());
        bytes[cursor + 8..cursor + 12].copy_from_slice(&count.to_le_bytes());
        cursor += RUN_HEADER;
        if flags == RLE_PAGES {
            bytes[cursor] = u8::from(uniform);
            cursor += 1;
        }
        let stored = if uniform { &rows[at..at + 1] } else { &rows[at..end] };
        for &(_, covered_value) in stored {
            let code = codes[&covered_value];
            if code_bytes == 1 {
                bytes[cursor] = code as u8;
                cursor += 1;
            } else {
                bytes[cursor..cursor + 2].copy_from_slice(&code.to_le_bytes());
                cursor += 2;
            }
        }
        page_rows = page_rows
            .checked_add(count)
            .ok_or_else(|| invalid("run projection page row count overflow"))?;
        at = end;
    }
    finish_page(&mut bytes, page, header, cursor, page_rows, page_first, page_last)?;
    let pages = u32::try_from(page + 1).map_err(|_| invalid("too many run projection pages"))?;
    bytes[22..26].copy_from_slice(&pages.to_le_bytes());
    drop(reader);
    drop(catalog);
    attach(
        path,
        table,
        &[section::Attachment {
            kind: *section::RUN_PROJECTION,
            id: id(order, covered)?,
            flags,
            header_bytes: header as u32,
            bytes: &bytes,
        }],
    )?;
    Ok(())
}

fn finish_page(
    bytes: &mut [u8],
    page: usize,
    header: usize,
    cursor: usize,
    rows: u32,
    first: Option<i64>,
    last: Option<i64>,
) -> Result<()> {
    let page_bytes = bytes.len() / (page + 1);
    let prefix = page * page_bytes + if page == 0 { header } else { 0 };
    let used = u32::try_from(cursor - prefix - PAGE_HEADER)
        .map_err(|_| invalid("run projection page length overflow"))?;
    bytes[prefix..prefix + 4].copy_from_slice(&used.to_le_bytes());
    bytes[prefix + 4..prefix + 8].copy_from_slice(&rows.to_le_bytes());
    bytes[prefix + 8..prefix + 16].copy_from_slice(&first.unwrap_or(0).to_le_bytes());
    bytes[prefix + 16..prefix + 24].copy_from_slice(&last.unwrap_or(0).to_le_bytes());
    Ok(())
}

#[derive(Debug)]
pub struct RunProjectionPart {
    counts: Vec<u64>,
    rows: u64,
    first: Option<i64>,
    last: Option<i64>,
}

#[derive(Debug)]
pub struct RunProjectionScan<'a> {
    reader: &'a Reader,
    extents: Vec<section::Extent>,
    first_page: Vec<u8>,
    dictionary: Vec<i32>,
    rows: u64,
    header: usize,
    code_bytes: usize,
    rle: bool,
}

#[derive(Clone, Copy)]
struct ScanLayout {
    header: usize,
    dictionary: usize,
    code_bytes: usize,
    rle: bool,
}

impl RunProjectionScan<'_> {
    #[must_use]
    pub fn pages(&self) -> usize {
        self.extents.len()
    }

    /// Scan one disjoint range of whole pages on an engine worker.
    pub fn partition(&self, part: usize, parts: usize) -> Result<RunProjectionPart> {
        if parts == 0 || part >= parts || parts > self.pages() {
            return Err(invalid("run projection partition is outside its pages"));
        }
        let begin = self.pages() * part / parts;
        let end = self.pages() * (part + 1) / parts;
        scan_pages(
            self.reader,
            &self.extents[begin..end],
            begin,
            ScanLayout {
                header: self.header,
                dictionary: self.dictionary.len(),
                code_bytes: self.code_bytes,
                rle: self.rle,
            },
            (begin == 0).then(|| self.first_page.clone()),
        )
    }

    /// Merge page ranges in their stored order and verify the complete row count.
    pub fn finish(
        &self,
        scans: impl IntoIterator<Item = RunProjectionPart>,
        limit: usize,
    ) -> Result<Vec<(i32, u64)>> {
        let mut totals = vec![0_u64; self.dictionary.len()];
        let mut total_rows = 0_u64;
        let mut previous_last = None;
        for scan in scans {
            if let Some(first) = scan.first {
                if previous_last.is_some_and(|previous| first <= previous) {
                    return Err(invalid("run projection page order differs"));
                }
                previous_last = scan.last;
            }
            total_rows = total_rows
                .checked_add(scan.rows)
                .ok_or_else(|| invalid("run projection row count overflow"))?;
            for (total, value) in totals.iter_mut().zip(scan.counts) {
                *total = total
                    .checked_add(value)
                    .ok_or_else(|| invalid("run projection count overflow"))?;
            }
        }
        if total_rows != self.rows {
            return Err(invalid("run projection decoded row count differs"));
        }
        let mut ranked = self
            .dictionary
            .iter()
            .copied()
            .zip(totals)
            .filter(|(_, count)| *count != 0)
            .collect::<Vec<_>>();
        ranked.sort_unstable_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
        ranked.truncate(limit);
        Ok(ranked)
    }
}

impl Reader {
    /// Whether a current row-preserving projection covers these columns.
    ///
    /// The payload is validated when it is read, not during this directory lookup.
    pub fn has_run_projection(&self, order: usize, covered: usize) -> Result<bool> {
        let wanted = id(order, covered)?;
        Ok(self.table().sections().iter().any(|section| {
            section.kind == *section::RUN_PROJECTION
                && section.id == wanted
                && section.usable(self.table().generation())
        }))
    }

    /// Count exact distinct order values by covered value from a current run projection.
    ///
    /// Returns `None` if no current matching section exists. Stored codes and run lengths are
    /// decoded when this query runs; the file contains no saved distinct pair or grouped count.
    ///
    /// # Errors
    ///
    /// If a matching section or its source directory is damaged.
    ///
    /// # Panics
    ///
    /// Fixed-width header decoding assumes the lengths checked immediately before it.
    pub fn grouped_distinct_run_projection(
        &self,
        order: usize,
        covered: usize,
        limit: usize,
    ) -> Result<Option<Vec<(i32, u64)>>> {
        let workers = std::thread::available_parallelism().map_or(1, usize::from);
        self.grouped_distinct_run_projection_with_workers(order, covered, limit, workers)
    }

    /// Evaluate a grouped distinct count from source rows with a bounded number of workers.
    /// This is the entry point for the SQL operator, whose parallelism is set by the engine.
    ///
    /// # Errors
    ///
    /// If the projection directory or payload is damaged.
    ///
    /// # Panics
    ///
    /// Fixed-width header decoding assumes the lengths checked immediately before it.
    pub fn grouped_distinct_run_projection_with_workers(
        &self,
        order: usize,
        covered: usize,
        limit: usize,
        workers: usize,
    ) -> Result<Option<Vec<(i32, u64)>>> {
        let Some(scan) = self.run_projection_scan(order, covered)? else {
            return Ok(None);
        };
        let workers = workers.clamp(1, 8).min(scan.pages());
        let parts = if workers == 1 {
            let mut scan = scan;
            let first_page = std::mem::take(&mut scan.first_page);
            let part = scan_pages(
                scan.reader,
                &scan.extents,
                0,
                ScanLayout {
                    header: scan.header,
                    dictionary: scan.dictionary.len(),
                    code_bytes: scan.code_bytes,
                    rle: scan.rle,
                },
                Some(first_page),
            )?;
            return Ok(Some(scan.finish([part], limit)?));
        } else {
            std::thread::scope(|scope| -> Result<Vec<RunProjectionPart>> {
                let handles = (0..workers)
                    .map(|part| {
                        let scan = &scan;
                        scope.spawn(move || scan.partition(part, workers))
                    })
                    .collect::<Vec<_>>();
                handles
                    .into_iter()
                    .map(|handle| {
                        handle.join().map_err(|_| invalid("run projection worker panicked"))?
                    })
                    .collect()
            })?
        };
        Ok(Some(scan.finish(parts, limit)?))
    }

    /// Open and validate a row-preserving projection for independently scheduled page scans.
    ///
    /// # Errors
    ///
    /// If the projection directory or payload is damaged.
    ///
    /// # Panics
    ///
    /// Fixed-width header decoding assumes the lengths checked immediately before it.
    pub fn run_projection_scan(
        &self,
        order: usize,
        covered: usize,
    ) -> Result<Option<RunProjectionScan<'_>>> {
        let wanted = id(order, covered)?;
        let Some(section) = self.table().sections().iter().find(|section| {
            section.kind == *section::RUN_PROJECTION
                && section.id == wanted
                && section.usable(self.table().generation())
        }) else {
            return Ok(None);
        };
        let page_bytes = match section.flags {
            0 => LEGACY_PAGE_BYTES,
            RLE_PAGES => RLE_PAGE_BYTES,
            _ => return Err(invalid("run projection has unknown page layout flags")),
        };
        let extents = self.extents(section)?;
        let first_extent = extents.first().ok_or_else(|| invalid("run projection has no page"))?;
        let first_page = self.extent(first_extent)?;
        if first_page.len() != page_bytes || &first_page[..8] != MAGIC {
            return Err(invalid("run projection header differs"));
        }
        let stored_order = u16::from_le_bytes(first_page[8..10].try_into().unwrap());
        let stored_covered = u16::from_le_bytes(first_page[10..12].try_into().unwrap());
        if usize::from(stored_order) != order || usize::from(stored_covered) != covered {
            return Err(invalid("run projection columns differ from its section"));
        }
        let rows = u64::from_le_bytes(first_page[12..20].try_into().unwrap());
        if rows != self.table().rows() as u64 {
            return Err(invalid("run projection row count differs from its table"));
        }
        let size = u16::from_le_bytes(first_page[20..22].try_into().unwrap()) as usize;
        let pages = u32::from_le_bytes(first_page[22..26].try_into().unwrap()) as usize;
        let code_bytes = usize::from(first_page[26]);
        if !matches!(code_bytes, 1 | 2) || (code_bytes == 1 && size > 256) {
            return Err(invalid("run projection code width differs from its dictionary"));
        }
        if pages != extents.len() {
            return Err(invalid("run projection page count differs from its extents"));
        }
        let header = FIXED_HEADER + size * 4;
        if header + PAGE_HEADER > page_bytes || section.header_bytes as usize != header {
            return Err(invalid("run projection dictionary exceeds its first page"));
        }
        let dictionary = first_page[FIXED_HEADER..header]
            .chunks_exact(4)
            .map(|bytes| i32::from_le_bytes(bytes.try_into().unwrap()))
            .collect::<Vec<_>>();
        if dictionary.windows(2).any(|pair| pair[0] >= pair[1]) {
            return Err(invalid("run projection dictionary is not sorted and unique"));
        }
        let base = first_extent.offset;
        for (at, extent) in extents.iter().enumerate() {
            let offset = at as u64 * page_bytes as u64;
            if extent.first != offset
                || extent.offset != base + offset
                || extent.length as usize != page_bytes
            {
                return Err(invalid("run projection pages are not contiguous"));
            }
        }
        Ok(Some(RunProjectionScan {
            reader: self,
            extents,
            first_page,
            dictionary,
            rows,
            header,
            code_bytes,
            rle: section.flags == RLE_PAGES,
        }))
    }
}

fn scan_pages(
    reader: &Reader,
    extents: &[section::Extent],
    first_index: usize,
    layout: ScanLayout,
    initial: Option<Vec<u8>>,
) -> Result<RunProjectionPart> {
    let ScanLayout { header, dictionary, code_bytes, rle } = layout;
    let page_bytes =
        extents.first().ok_or_else(|| invalid("run projection has no page"))?.length as usize;
    let mut marks = vec![0_u32; dictionary];
    let mut counts = vec![0_u64; dictionary];
    let mut epoch = 0_u32;
    let mut rows = 0_u64;
    let mut first = None;
    let mut last = None;
    let reused_first = initial.is_some();
    let mut bytes = initial.unwrap_or_else(|| Vec::with_capacity(page_bytes));
    for (relative, extent) in extents.iter().enumerate() {
        if !reused_first || relative != 0 {
            reader.extent_into(extent, &mut bytes)?;
        }
        let prefix = if first_index + relative == 0 { header } else { 0 };
        if bytes.len() != page_bytes || prefix + PAGE_HEADER > page_bytes {
            return Err(invalid("run projection page length differs"));
        }
        let used = u32::from_le_bytes(bytes[prefix..prefix + 4].try_into().unwrap()) as usize;
        let stored_rows =
            u32::from_le_bytes(bytes[prefix + 4..prefix + 8].try_into().unwrap()) as u64;
        let stored_first = i64::from_le_bytes(bytes[prefix + 8..prefix + 16].try_into().unwrap());
        let stored_last = i64::from_le_bytes(bytes[prefix + 16..prefix + 24].try_into().unwrap());
        let mut at = prefix + PAGE_HEADER;
        let end = at
            .checked_add(used)
            .filter(|&end| end <= page_bytes)
            .ok_or_else(|| invalid("run projection page data exceeds its extent"))?;
        let mut page_rows = 0_u64;
        let mut page_first = None;
        let mut page_last = None;
        while at < end {
            if end - at < RUN_HEADER {
                return Err(invalid("run projection page has a partial run header"));
            }
            let user = i64::from_le_bytes(bytes[at..at + 8].try_into().unwrap());
            let length = u32::from_le_bytes(bytes[at + 8..at + 12].try_into().unwrap()) as usize;
            at += RUN_HEADER;
            let mode = if rle {
                if at == end {
                    return Err(invalid("run projection run is missing its mode"));
                }
                let mode = bytes[at];
                at += 1;
                mode
            } else {
                0
            };
            let payload_bytes = match mode {
                0 => length * code_bytes,
                1 => code_bytes,
                _ => return Err(invalid("run projection run has an unknown mode")),
            };
            if length == 0 || payload_bytes > end - at {
                return Err(invalid("run projection run length exceeds its page"));
            }
            if last.is_some_and(|previous| user <= previous) {
                return Err(invalid("run projection order is not increasing"));
            }
            first.get_or_insert(user);
            page_first.get_or_insert(user);
            page_last = Some(user);
            last = Some(user);
            if mode == 1 {
                let code = if code_bytes == 1 {
                    usize::from(bytes[at])
                } else {
                    u16::from_le_bytes(bytes[at..at + 2].try_into().unwrap()) as usize
                };
                let count = counts
                    .get_mut(code)
                    .ok_or_else(|| invalid("run projection code is outside its dictionary"))?;
                *count += 1;
            } else if code_bytes == 1 {
                epoch = epoch.wrapping_add(1);
                if epoch == 0 {
                    marks.fill(0);
                    epoch = 1;
                }
                for &code in &bytes[at..at + length] {
                    let code = usize::from(code);
                    let mark = marks
                        .get_mut(code)
                        .ok_or_else(|| invalid("run projection code is outside its dictionary"))?;
                    if *mark != epoch {
                        *mark = epoch;
                        counts[code] += 1;
                    }
                }
            } else {
                if length <= 2 {
                    let first_code =
                        u16::from_le_bytes(bytes[at..at + 2].try_into().unwrap()) as usize;
                    let first_count = counts
                        .get_mut(first_code)
                        .ok_or_else(|| invalid("run projection code is outside its dictionary"))?;
                    *first_count += 1;
                    if length == 2 {
                        let second_code =
                            u16::from_le_bytes(bytes[at + 2..at + 4].try_into().unwrap()) as usize;
                        if second_code != first_code {
                            let second_count = counts.get_mut(second_code).ok_or_else(|| {
                                invalid("run projection code is outside its dictionary")
                            })?;
                            *second_count += 1;
                        }
                    }
                } else {
                    epoch = epoch.wrapping_add(1);
                    if epoch == 0 {
                        marks.fill(0);
                        epoch = 1;
                    }
                    for code_bytes in bytes[at..at + length * 2].chunks_exact(2) {
                        let code = u16::from_le_bytes(code_bytes.try_into().unwrap()) as usize;
                        let mark = marks.get_mut(code).ok_or_else(|| {
                            invalid("run projection code is outside its dictionary")
                        })?;
                        if *mark != epoch {
                            *mark = epoch;
                            counts[code] += 1;
                        }
                    }
                }
            }
            at += payload_bytes;
            page_rows += length as u64;
        }
        if page_rows != stored_rows
            || page_first.unwrap_or(0) != stored_first
            || page_last.unwrap_or(0) != stored_last
        {
            return Err(invalid("run projection page directory differs from its rows"));
        }
        rows = rows
            .checked_add(page_rows)
            .ok_or_else(|| invalid("run projection row count overflow"))?;
    }
    Ok(RunProjectionPart { counts, rows, first, last })
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicUsize, Ordering};

    use rudb_common::{Field, LogicalType, Value};
    use rudb_vector::{Chunk, Vector};

    use crate::{Catalog, Writer};

    use super::{RLE_PAGES, build_run_projection};

    static NEXT: AtomicUsize = AtomicUsize::new(0);

    #[test]
    fn run_projection_keeps_duplicate_rows_but_counts_distinct_pairs() {
        let at = NEXT.fetch_add(1, Ordering::Relaxed);
        let path = std::env::temp_dir()
            .join(format!("rudb-run-projection-{}-{at}.rdb", std::process::id()));
        let fields = vec![
            Field::required("user", LogicalType::BigInt),
            Field::required("region", LogicalType::Integer),
        ];
        let mut writer = Writer::create(&path, "events", fields).expect("create native file");
        for (users, regions) in [
            (vec![9_i64, 2, 9, 1], vec![7_i32, 1, 7, 2]),
            (vec![2_i64, 2, 5, 9, 5, 8, 8], vec![2_i32, 2, 1, 1, 2, 1, 1]),
        ] {
            let users = users.into_iter().map(Value::BigInt).collect::<Vec<_>>();
            let regions = regions.into_iter().map(Value::Integer).collect::<Vec<_>>();
            let chunk = Chunk::new(vec![
                Vector::from_values(LogicalType::BigInt, &users).expect("users"),
                Vector::from_values(LogicalType::Integer, &regions).expect("regions"),
            ])
            .expect("matching columns");
            writer.append(&chunk).expect("append rows");
        }
        writer.finish().expect("commit native file");
        build_run_projection(&path, "events", "user", "region").expect("build run projection");
        let reader = Catalog::open(&path).expect("catalog").table("events").expect("table");
        assert!(reader.table().sections().iter().any(|section| {
            section.kind == *crate::section::RUN_PROJECTION && section.flags == RLE_PAGES
        }));
        assert_eq!(
            reader.grouped_distinct_run_projection(0, 1, 10).expect("valid projection"),
            Some(vec![(1, 4), (2, 3), (7, 1)]),
        );
        let scan = reader.run_projection_scan(0, 1).expect("open projection").expect("projection");
        let mut reconstructed = Vec::new();
        for (page, extent) in scan.extents.iter().enumerate() {
            let bytes = reader.extent(extent).expect("read verified page");
            let prefix = if page == 0 { scan.header } else { 0 };
            let used = u32::from_le_bytes(bytes[prefix..prefix + 4].try_into().unwrap()) as usize;
            let mut cursor = prefix + super::PAGE_HEADER;
            let end = cursor + used;
            while cursor < end {
                let user = i64::from_le_bytes(bytes[cursor..cursor + 8].try_into().unwrap());
                let length =
                    u32::from_le_bytes(bytes[cursor + 8..cursor + 12].try_into().unwrap()) as usize;
                cursor += super::RUN_HEADER;
                let mode = if scan.rle {
                    let mode = bytes[cursor];
                    cursor += 1;
                    mode
                } else {
                    0
                };
                let codes = if mode == 1 { 1 } else { length };
                let mut covered = Vec::with_capacity(codes);
                for _ in 0..codes {
                    let code = if scan.code_bytes == 1 {
                        let code = usize::from(bytes[cursor]);
                        cursor += 1;
                        code
                    } else {
                        let code = u16::from_le_bytes(bytes[cursor..cursor + 2].try_into().unwrap())
                            as usize;
                        cursor += 2;
                        code
                    };
                    covered.push(scan.dictionary[code]);
                }
                if mode == 1 {
                    reconstructed.extend(std::iter::repeat_n((user, covered[0]), length));
                } else {
                    reconstructed.extend(covered.into_iter().map(|value| (user, value)));
                }
            }
        }
        reconstructed.sort_unstable();
        assert_eq!(
            reconstructed,
            vec![
                (1, 2),
                (2, 1),
                (2, 2),
                (2, 2),
                (5, 1),
                (5, 2),
                (8, 1),
                (8, 1),
                (9, 1),
                (9, 7),
                (9, 7),
            ]
        );
        std::fs::remove_file(path).expect("remove scratch file");
    }

    #[test]
    fn run_projection_uses_wide_codes_for_large_dictionaries() {
        let at = NEXT.fetch_add(1, Ordering::Relaxed);
        let path = std::env::temp_dir()
            .join(format!("rudb-run-projection-wide-{}-{at}.rdb", std::process::id()));
        let fields = vec![
            Field::required("user", LogicalType::BigInt),
            Field::required("region", LogicalType::Integer),
        ];
        let users = vec![Value::BigInt(7); 300];
        let regions = (0..300).map(Value::Integer).collect::<Vec<_>>();
        let chunk = Chunk::new(vec![
            Vector::from_values(LogicalType::BigInt, &users).expect("users"),
            Vector::from_values(LogicalType::Integer, &regions).expect("regions"),
        ])
        .expect("matching columns");
        let mut writer = Writer::create(&path, "events", fields).expect("create native file");
        writer.append(&chunk).expect("append rows");
        writer.finish().expect("commit native file");
        build_run_projection(&path, "events", "user", "region").expect("build run projection");
        let reader = Catalog::open(&path).expect("catalog").table("events").expect("table");
        let expected = (0..300).map(|region| (region, 1)).collect::<Vec<_>>();
        assert_eq!(
            reader.grouped_distinct_run_projection(0, 1, 300).expect("valid projection"),
            Some(expected),
        );
        std::fs::remove_file(path).expect("remove scratch file");
    }

    #[test]
    fn run_projection_page_partitions_merge_exact_distinct_counts() {
        let at = NEXT.fetch_add(1, Ordering::Relaxed);
        let path = std::env::temp_dir()
            .join(format!("rudb-run-projection-pages-{}-{at}.rdb", std::process::id()));
        let fields = vec![
            Field::required("user", LogicalType::BigInt),
            Field::required("region", LogicalType::Integer),
        ];
        let mut writer = Writer::create(&path, "events", fields).expect("create native file");
        for batch in 0..150 {
            let users = (0..1000)
                .map(|row| Value::BigInt(i64::from(batch * 500 + row / 2)))
                .collect::<Vec<_>>();
            let regions = (0..1000).map(|row| Value::Integer(row % 2 + 1)).collect::<Vec<_>>();
            writer
                .append(
                    &Chunk::new(vec![
                        Vector::from_values(LogicalType::BigInt, &users).expect("users"),
                        Vector::from_values(LogicalType::Integer, &regions).expect("regions"),
                    ])
                    .expect("matching columns"),
                )
                .expect("append rows");
        }
        writer.finish().expect("commit native file");
        build_run_projection(&path, "events", "user", "region").expect("build run projection");
        let reader = Catalog::open(&path).expect("catalog").table("events").expect("table");
        let scan = reader.run_projection_scan(0, 1).expect("open projection").expect("projection");
        assert!(scan.pages() > 1);
        let left = scan.partition(0, 2).expect("left pages");
        let right = scan.partition(1, 2).expect("right pages");
        assert_eq!(
            scan.finish([left, right], usize::MAX).expect("merge pages"),
            vec![(1, 75_000), (2, 75_000)]
        );
        std::fs::remove_file(path).expect("remove scratch file");
    }

    #[test]
    fn long_order_runs_keep_the_legacy_page_width() {
        let at = NEXT.fetch_add(1, Ordering::Relaxed);
        let path = std::env::temp_dir()
            .join(format!("rudb-run-projection-long-{}-{at}.rdb", std::process::id()));
        let fields = vec![
            Field::required("user", LogicalType::BigInt),
            Field::required("region", LogicalType::Integer),
        ];
        let users = vec![Value::BigInt(7); 1000];
        let regions = vec![Value::Integer(1); 1000];
        let chunk = Chunk::new(vec![
            Vector::from_values(LogicalType::BigInt, &users).expect("users"),
            Vector::from_values(LogicalType::Integer, &regions).expect("regions"),
        ])
        .expect("matching columns");
        let mut writer = Writer::create(&path, "events", fields).expect("create native file");
        for _ in 0..150 {
            writer.append(&chunk).expect("append rows");
        }
        writer.finish().expect("commit native file");
        build_run_projection(&path, "events", "user", "region").expect("build run projection");
        let reader = Catalog::open(&path).expect("catalog").table("events").expect("table");
        assert!(reader.table().sections().iter().any(|section| {
            section.kind == *crate::section::RUN_PROJECTION && section.flags == 0
        }));
        assert_eq!(
            reader.grouped_distinct_run_projection(0, 1, 10).expect("valid projection"),
            Some(vec![(1, 1)])
        );
        std::fs::remove_file(path).expect("remove scratch file");
    }
}
