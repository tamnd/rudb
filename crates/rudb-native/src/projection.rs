//! Optional row-valued projection ordered by one signed integer column.
//!
//! The first format covers a second signed integer column with a small dictionary. It carries
//! every row in order, never a grouped result. A table append changes the table generation and
//! makes the attached section unusable until it is rebuilt.

use std::collections::{HashMap, HashSet};
use std::path::Path;

use rudb_common::{LogicalType, Result};
use rudb_vector::Vector;

use crate::{Catalog, Reader, attach, invalid, section};

const MAGIC: &[u8; 8] = b"RUDBSP1\0";
const FIXED_HEADER: usize = 8 + 2 + 2 + 8 + 2;
const ROW_BYTES: usize = 10;

pub(crate) fn id(order: usize, covered: usize) -> Result<u64> {
    let order =
        u32::try_from(order).map_err(|_| invalid("projection order column is too large"))?;
    let covered =
        u32::try_from(covered).map_err(|_| invalid("projection covered column is too large"))?;
    Ok((u64::from(order) << 32) | u64::from(covered))
}

/// Every value of a non-null signed integer column, as a block where the vector hands one over and
/// through `signed_at` for the forms it does not.
pub(crate) fn integers(vector: &Vector, out: &mut Vec<i64>) -> Result<()> {
    if vector.signed_block(out) && out.len() == vector.len() {
        return Ok(());
    }
    out.clear();
    // row at a time: the run and compressed forms have no block to hand over.
    for row in 0..vector.len() {
        let value = vector
            .signed_at(row)
            .and_then(|value| i64::try_from(value).ok())
            .ok_or_else(|| invalid("sorted projection requires non-null signed integers"))?;
        out.push(value);
    }
    Ok(())
}

pub(crate) fn eligible(order: &LogicalType, covered: &LogicalType) -> bool {
    matches!(
        order,
        LogicalType::TinyInt | LogicalType::SmallInt | LogicalType::Integer | LogicalType::BigInt
    ) && matches!(covered, LogicalType::TinyInt | LogicalType::SmallInt | LogicalType::Integer)
}

/// Build a row-valued, sorted, covering projection inside an existing native file.
///
/// The caller chooses columns by name. This is an explicit storage operation whose build time,
/// peak memory, and bytes must be charged to the workload that asks for it. No SQL aggregate is
/// evaluated or stored. A later append makes the section stale until this is called again.
///
/// # Errors
///
/// If the table or columns are missing, nullable, unsupported, or the file cannot be read or
/// updated. More than 65,535 covered values exceed this first format's dictionary.
pub fn build_sorted_projection(
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
        .ok_or_else(|| invalid("projection order column is missing"))?;
    let covered = fields
        .iter()
        .position(|field| field.name.eq_ignore_ascii_case(covered_column))
        .ok_or_else(|| invalid("projection covered column is missing"))?;
    if order == covered
        || !fields[order].not_null
        || !fields[covered].not_null
        || !eligible(&fields[order].ty, &fields[covered].ty)
    {
        return Err(invalid("sorted projection needs two supported, non-null integer columns"));
    }
    let mut rows = Vec::<(i64, i32)>::with_capacity(reader.table().rows());
    let mut values = HashSet::<i32>::new();
    let (mut users, mut groups) = (Vec::new(), Vec::new());
    for part in 0..reader.parts() {
        let chunk = reader.read(part, &[order, covered])?;
        integers(chunk.column(0)?, &mut users)?;
        integers(chunk.column(1)?, &mut groups)?;
        for (&user, &group) in users.iter().zip(&groups) {
            let group = i32::try_from(group)
                .map_err(|_| invalid("projection covered value exceeds INTEGER"))?;
            rows.push((user, group));
            values.insert(group);
        }
    }
    if rows.len() != reader.table().rows() {
        return Err(invalid("projection source row count differs from its table"));
    }
    let mut dictionary = values.into_iter().collect::<Vec<_>>();
    dictionary.sort_unstable();
    let dictionary_len = u16::try_from(dictionary.len())
        .map_err(|_| invalid("projection covered column exceeds 65535 values"))?;
    let codes = dictionary
        .iter()
        .enumerate()
        .map(|(at, &value)| (value, at as u16))
        .collect::<HashMap<_, _>>();
    rows.sort_unstable_by_key(|&(user, _)| user);
    let order_index =
        u16::try_from(order).map_err(|_| invalid("projection column index overflow"))?;
    let covered_index =
        u16::try_from(covered).map_err(|_| invalid("projection column index overflow"))?;
    let header = FIXED_HEADER + dictionary.len() * 4;
    let mut bytes = Vec::with_capacity(header + rows.len() * ROW_BYTES);
    bytes.extend_from_slice(MAGIC);
    bytes.extend_from_slice(&order_index.to_le_bytes());
    bytes.extend_from_slice(&covered_index.to_le_bytes());
    bytes.extend_from_slice(&(rows.len() as u64).to_le_bytes());
    bytes.extend_from_slice(&dictionary_len.to_le_bytes());
    for value in &dictionary {
        bytes.extend_from_slice(&value.to_le_bytes());
    }
    for (user, group) in rows {
        bytes.extend_from_slice(&user.to_le_bytes());
        bytes.extend_from_slice(&codes[&group].to_le_bytes());
    }
    drop(reader);
    drop(catalog);
    attach(
        path,
        table,
        &[section::Attachment {
            kind: *section::SORTED_PROJECTION,
            id: id(order, covered)?,
            flags: 0,
            header_bytes: header as u32,
            bytes: &bytes,
        }],
    )?;
    Ok(())
}

impl Reader {
    /// Count distinct order values by covered value from a current sorted projection.
    ///
    /// Returns `None` when no matching current projection exists. Workers split only between
    /// order values, so each `(order, covered)` pair is counted once without a global hash set.
    ///
    /// # Errors
    ///
    /// If a matching projection or its source directory is damaged.
    ///
    /// # Panics
    ///
    /// Fixed-width header decoding assumes the header length checked immediately before it.
    pub fn grouped_distinct_projection(
        &self,
        order: usize,
        covered: usize,
        limit: usize,
    ) -> Result<Option<Vec<(i32, u64)>>> {
        let wanted = id(order, covered)?;
        let Some(section) = self.table().sections().iter().find(|section| {
            section.kind == *section::SORTED_PROJECTION
                && section.id == wanted
                && section.usable(self.table().generation())
        }) else {
            return Ok(None);
        };
        let extents = self.extents(section)?;
        let first = extents.first().ok_or_else(|| invalid("projection has no extent"))?;
        let bytes = self.extent(first)?;
        if bytes.len() < FIXED_HEADER || &bytes[..8] != MAGIC {
            return Err(invalid("projection header differs"));
        }
        let stored_order = u16::from_le_bytes(bytes[8..10].try_into().unwrap());
        let stored_covered = u16::from_le_bytes(bytes[10..12].try_into().unwrap());
        if usize::from(stored_order) != order || usize::from(stored_covered) != covered {
            return Err(invalid("projection columns differ from its section"));
        }
        let rows = u64::from_le_bytes(bytes[12..20].try_into().unwrap());
        if rows != self.table().rows() as u64 {
            return Err(invalid("projection row count differs from its table"));
        }
        let size = u16::from_le_bytes(bytes[20..22].try_into().unwrap()) as usize;
        let header = FIXED_HEADER + size * 4;
        if bytes.len() < header || section.header_bytes as usize != header {
            return Err(invalid("projection dictionary exceeds its first extent"));
        }
        let dictionary = bytes[FIXED_HEADER..header]
            .chunks_exact(4)
            .map(|item| i32::from_le_bytes(item.try_into().unwrap()))
            .collect::<Vec<_>>();
        if dictionary.windows(2).any(|pair| pair[0] >= pair[1]) {
            return Err(invalid("projection dictionary is not sorted and unique"));
        }
        let expected = (header as u64)
            .checked_add(
                rows.checked_mul(ROW_BYTES as u64)
                    .ok_or_else(|| invalid("projection length overflow"))?,
            )
            .ok_or_else(|| invalid("projection length overflow"))?;
        let base = first.offset;
        let mut consumed = 0_u64;
        for extent in &extents {
            if extent.first != consumed || extent.offset != base + consumed {
                return Err(invalid("projection extents are not contiguous"));
            }
            consumed += u64::from(extent.length);
        }
        if consumed != expected {
            return Err(invalid("projection byte length differs from its row count"));
        }
        let rows = usize::try_from(rows).map_err(|_| invalid("projection rows exceed memory"))?;
        let workers = if rows < 2_000_000 {
            1
        } else {
            std::thread::available_parallelism().map_or(1, usize::from).min(8).min(rows)
        };
        let mut boundaries = Vec::with_capacity(workers + 1);
        boundaries.push(0);
        for worker in 1..workers {
            let mut at = rows * worker / workers;
            if at > 0 && at < rows {
                let previous = projected_user(self, base, header, at - 1)?;
                while at < rows && projected_user(self, base, header, at)? == previous {
                    at += 1;
                }
            }
            boundaries.push(at);
        }
        boundaries.push(rows);
        let counts = std::thread::scope(|scope| -> Result<Vec<Vec<u64>>> {
            let mut handles = Vec::with_capacity(workers);
            for pair in boundaries.windows(2) {
                let (start, end) = (pair[0], pair[1]);
                let extents = &extents;
                handles
                    .push(scope.spawn(move || scan_range(self, extents, header, size, start, end)));
            }
            handles
                .into_iter()
                .map(|handle| handle.join().map_err(|_| invalid("projection worker panicked"))?)
                .collect()
        })?;
        let mut totals = vec![0_u64; size];
        for local in counts {
            for (total, value) in totals.iter_mut().zip(local) {
                *total =
                    total.checked_add(value).ok_or_else(|| invalid("projection count overflow"))?;
            }
        }
        let mut ranked =
            dictionary.into_iter().zip(totals).filter(|(_, count)| *count != 0).collect::<Vec<_>>();
        ranked.sort_unstable_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
        ranked.truncate(limit);
        Ok(Some(ranked))
    }
}

fn projected_user(reader: &Reader, base: u64, header: usize, row: usize) -> Result<i64> {
    let mut bytes = [0_u8; 8];
    let offset = base + header as u64 + row as u64 * ROW_BYTES as u64;
    crate::read_at(&reader.file, offset, &mut bytes)?;
    Ok(i64::from_le_bytes(bytes))
}

fn scan_range(
    reader: &Reader,
    extents: &[section::Extent],
    header: usize,
    dictionary: usize,
    start: usize,
    end: usize,
) -> Result<Vec<u64>> {
    let low = header as u64 + start as u64 * ROW_BYTES as u64;
    let high = header as u64 + end as u64 * ROW_BYTES as u64;
    let mut marks = vec![0_u64; dictionary];
    let mut counts = vec![0_u64; dictionary];
    let mut current_user = None;
    let mut epoch = 0_u64;
    let mut seen = 0_usize;
    let mut carry = [0_u8; ROW_BYTES];
    let mut carry_len = 0_usize;
    for extent in extents {
        let extent_end = extent.first + u64::from(extent.length);
        if extent_end <= low || extent.first >= high {
            continue;
        }
        let bytes = reader.extent(extent)?;
        let begin = low.saturating_sub(extent.first) as usize;
        let finish = (high.min(extent_end) - extent.first) as usize;
        let mut block = &bytes[begin..finish];
        if carry_len != 0 {
            let needed = ROW_BYTES - carry_len;
            let taken = needed.min(block.len());
            carry[carry_len..carry_len + taken].copy_from_slice(&block[..taken]);
            carry_len += taken;
            block = &block[taken..];
            if carry_len < ROW_BYTES {
                continue;
            }
            process_block(
                &carry,
                dictionary,
                &mut current_user,
                &mut epoch,
                &mut marks,
                &mut counts,
            )?;
            seen += 1;
        }
        let chunks = block.chunks_exact(ROW_BYTES);
        let remainder = chunks.remainder();
        for chunk in chunks {
            process_block(
                chunk,
                dictionary,
                &mut current_user,
                &mut epoch,
                &mut marks,
                &mut counts,
            )?;
            seen += 1;
        }
        carry[..remainder.len()].copy_from_slice(remainder);
        carry_len = remainder.len();
    }
    if carry_len != 0 || seen != end - start {
        return Err(invalid("projection scan did not cover its range"));
    }
    Ok(counts)
}

#[inline(always)]
fn process_block(
    bytes: &[u8],
    dictionary: usize,
    current_user: &mut Option<i64>,
    epoch: &mut u64,
    marks: &mut [u64],
    counts: &mut [u64],
) -> Result<()> {
    let user = i64::from_le_bytes(bytes[..8].try_into().unwrap());
    let code = u16::from_le_bytes(bytes[8..10].try_into().unwrap()) as usize;
    if code >= dictionary {
        return Err(invalid("projection code is outside its dictionary"));
    }
    if current_user.is_some_and(|previous| user < previous) {
        return Err(invalid("projection order is descending"));
    }
    if *current_user != Some(user) {
        *epoch = epoch.checked_add(1).ok_or_else(|| invalid("projection epoch overflow"))?;
        *current_user = Some(user);
    }
    if marks[code] != *epoch {
        marks[code] = *epoch;
        counts[code] += 1;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::collections::{HashMap, HashSet};
    use std::sync::atomic::{AtomicUsize, Ordering};

    use rudb_common::{Field, LogicalType, Value};
    use rudb_vector::{Chunk, Vector};

    use crate::{Catalog, Writer};

    use super::build_sorted_projection;

    static NEXT: AtomicUsize = AtomicUsize::new(0);

    #[test]
    fn sorted_projection_counts_each_cover_value_once_per_order_value() {
        let at = NEXT.fetch_add(1, Ordering::Relaxed);
        let path = std::env::temp_dir()
            .join(format!("rudb-sorted-projection-{}-{at}.rdb", std::process::id()));
        let fields = vec![
            Field::required("user", LogicalType::BigInt),
            Field::required("region", LogicalType::Integer),
        ];
        let mut writer = Writer::create(&path, "events", fields).expect("create native file");
        for (users, regions) in
            [(vec![9_i64, 2, 9], vec![7_i32, 1, 7]), (vec![2_i64, 2, 5], vec![2_i32, 2, 1])]
        {
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
        let reader = Catalog::open(&path).expect("catalog").table("events").expect("table");
        assert_eq!(reader.grouped_distinct_projection(0, 1, 10).expect("no projection"), None);
        drop(reader);
        build_sorted_projection(&path, "events", "user", "region").expect("build projection");
        let reader = Catalog::open(&path).expect("catalog").table("events").expect("table");
        assert_eq!(
            reader.grouped_distinct_projection(0, 1, 10).expect("valid projection"),
            Some(vec![(1, 2), (2, 1), (7, 1)]),
        );
        assert_eq!(reader.grouped_distinct_projection(1, 0, 10).expect("different order"), None);
        std::fs::remove_file(path).expect("remove scratch file");
    }

    #[test]
    fn sorted_projection_crosses_extent_and_worker_boundaries() {
        let at = NEXT.fetch_add(1, Ordering::Relaxed);
        let path = std::env::temp_dir()
            .join(format!("rudb-sorted-projection-wide-{}-{at}.rdb", std::process::id()));
        let fields = vec![
            Field::required("user", LogicalType::BigInt),
            Field::required("region", LogicalType::Integer),
        ];
        let mut writer = Writer::create(&path, "events", fields).expect("create native file");
        let mut pairs = HashSet::new();
        for first in (0..75_000_i64).step_by(1_000) {
            let source = (first..first + 1_000)
                .map(|row| (row * 17 % 20_000, (row % 7) as i32))
                .collect::<Vec<_>>();
            pairs.extend(source.iter().copied());
            let users = source.iter().map(|(user, _)| Value::BigInt(*user)).collect::<Vec<_>>();
            let regions =
                source.iter().map(|(_, region)| Value::Integer(*region)).collect::<Vec<_>>();
            let chunk = Chunk::new(vec![
                Vector::from_values(LogicalType::BigInt, &users).expect("users"),
                Vector::from_values(LogicalType::Integer, &regions).expect("regions"),
            ])
            .expect("matching columns");
            writer.append(&chunk).expect("append rows");
        }
        writer.finish().expect("commit native file");
        build_sorted_projection(&path, "events", "user", "region").expect("build projection");
        let reader = Catalog::open(&path).expect("catalog").table("events").expect("table");
        let mut expected = HashMap::<i32, u64>::new();
        for (_, region) in pairs {
            *expected.entry(region).or_default() += 1;
        }
        let mut expected = expected.into_iter().collect::<Vec<_>>();
        expected.sort_unstable_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
        assert_eq!(
            reader.grouped_distinct_projection(0, 1, 10).expect("valid projection"),
            Some(expected)
        );
        std::fs::remove_file(path).expect("remove scratch file");
    }
}
