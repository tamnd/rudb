//! The keys of a table's file sealed into a `RUDBKI1` run at checkpoint, so that the first lookup
//! by key after a table is opened reads the run rather than the key of every row in the file.
//!
//! A checkpoint writes one section of kind [`section::KEY_RUNS`] for each key and plain unique index
//! of a table with at least [`FEWEST_ROWS`] rows, under the id [`id`] gives its columns. The payload
//! is, little endian, the rows of the file the run covers as a `u64`, the count of the key's columns
//! as a `u32` and each column as a `u32`, then the run (see `rudb_index::run`). Each entry is the
//! normalized key of one row with the row's place in the file as its rid. A row gone when the run
//! was written has no entry, and neither has a row with a null in its key.
//!
//! The rows a checkpoint appended to the file since are the ones the run does not cover, and a
//! lookup reads their keys the way it reads every key of a table with no run. The rows gone since
//! keep their entries, and a lookup passes over them by place. Once the rows not covered are a
//! quarter of the table, the next checkpoint writes the run again.

use rudb_common::{LogicalType, Result, Value};
use rudb_index::{Hit, Run, RunWriter, normalize};
use rudb_native::{Reader, section};

use crate::gone::Gone;
use crate::keys::{Encoded, Key, int_block};

/// Tables with fewer rows than this get no run. Reading every key of one costs less than a
/// millisecond.
pub const FEWEST_ROWS: usize = 1 << 16;

/// The bytes of the payload before the columns: the rows covered and the count of columns.
const HEAD: usize = 12;

/// The id of the section for the key over `columns`, in the key's own order.
#[must_use]
pub fn id(columns: &[usize]) -> u64 {
    let mut hash = 0xcbf2_9ce4_8422_2325_u64;
    for &column in columns {
        for byte in (column as u64).to_le_bytes() {
            hash = (hash ^ u64::from(byte)).wrapping_mul(0x0100_0000_01b3);
        }
    }
    hash
}

/// The normalized key of one `INTEGER` or `BIGINT`, the bytes `rudb_index::key::push` writes for it.
fn int_key(key: i64) -> [u8; 9] {
    let mut out = [1; 9];
    out[1..].copy_from_slice(&(key.cast_unsigned() ^ (1 << 63)).to_be_bytes());
    out
}

/// The `INTEGER` or `BIGINT` a normalized key of one column holds, when it holds one.
fn int_of(key: &[u8]) -> Option<i64> {
    let (&1, rest) = key.split_first()? else { return None };
    let bits: [u8; 8] = rest.try_into().ok()?;
    Some((u64::from_be_bytes(bits) ^ (1 << 63)).cast_signed())
}

/// The text a normalized key of one `VARCHAR` column holds, when it holds one.
fn text_of(key: &[u8]) -> Option<Box<[u8]>> {
    let (&1, rest) = key.split_first()? else { return None };
    let (&0, rest) = rest.split_last()? else { return None };
    let mut text = Vec::with_capacity(rest.len());
    let mut bytes = rest.iter();
    while let Some(&byte) = bytes.next() {
        text.push(byte);
        if byte == 0 && bytes.next() != Some(&0xFF) {
            return None;
        }
    }
    Some(text.into())
}

/// The rows of the file `reader` reads, every part of it, gone rows included.
fn file_rows(reader: &Reader) -> u64 {
    (0..reader.parts()).map(|part| reader.part_rows(part) as u64).sum()
}

/// Whether a key over columns of these types is one a run is written for. A float key compares
/// its own way and a lookup never asks for one, see `points::looks_up`.
fn sealable(types: &[&LogicalType]) -> bool {
    types.iter().all(|ty| !matches!(ty, LogicalType::Float | LogicalType::Double))
}

/// The payload of the section for the key over `columns` of the table `reader` reads, or `None`
/// when the table has too few rows, a column of the key was written over since the file was, or a
/// value of it cannot be normalized.
///
/// # Errors
///
/// If a part of the file cannot be read.
pub fn seal(reader: &Reader, columns: &[usize]) -> Result<Option<Vec<u8>>> {
    let fields = reader.table().fields();
    let Some(types) =
        columns.iter().map(|&at| fields.get(at).map(|field| &field.ty)).collect::<Option<Vec<_>>>()
    else {
        return Ok(None);
    };
    let covered = file_rows(reader);
    if covered < FEWEST_ROWS as u64 || columns.is_empty() || !sealable(&types) {
        return Ok(None);
    }
    let gone = reader.gone().map(|stored| Gone::stored(reader.parts(), stored)).transpose()?;
    if gone.as_ref().is_some_and(|gone| columns.iter().any(|&column| gone.touched(column))) {
        return Ok(None);
    }
    let is_gone =
        |part: usize, row: usize| gone.as_ref().is_some_and(|gone| gone.contains(part, row));
    let mut writer = RunWriter::new();
    if let [LogicalType::Integer | LogicalType::BigInt] = types.as_slice() {
        let projected = Key { columns: vec![0], primary: false };
        let mut keys: Vec<(i64, u64)> = Vec::with_capacity(covered as usize);
        let mut block = Vec::new();
        let mut first = 0_u64;
        for part in 0..reader.parts() {
            let chunk = reader.read(part, columns)?;
            if int_block(&chunk, &projected, &mut block)? {
                for (row, &key) in block.iter().enumerate() {
                    if !is_gone(part, row) {
                        keys.push((key, first + row as u64));
                    }
                }
            } else {
                let column = chunk.column(0)?;
                for row in 0..chunk.len() {
                    let key = match column.value_at(row) {
                        Value::Integer(key) => i64::from(key),
                        Value::BigInt(key) => key,
                        _ => continue,
                    };
                    if !is_gone(part, row) {
                        keys.push((key, first + row as u64));
                    }
                }
            }
            first += reader.part_rows(part) as u64;
        }
        keys.sort_unstable();
        for (key, rid) in keys {
            writer.push(&int_key(key), Hit { rid, ts: 0 })?;
        }
    } else {
        // Every key one after another in `bytes`, each with where it starts, how long it is and
        // its row, so that ten million of them are three allocations and not ten million.
        let mut bytes = Vec::new();
        let mut keys: Vec<(usize, usize, u64)> = Vec::with_capacity(covered as usize);
        let mut values = Vec::with_capacity(columns.len());
        let mut first = 0_u64;
        for part in 0..reader.parts() {
            let chunk = reader.read(part, columns)?;
            'rows: for row in 0..chunk.len() {
                if is_gone(part, row) {
                    continue;
                }
                values.clear();
                for column in 0..columns.len() {
                    let value = chunk.column(column)?.value_at(row);
                    if value.is_null() {
                        continue 'rows;
                    }
                    values.push(value);
                }
                let start = bytes.len();
                if !normalize(&values, &mut bytes) {
                    return Ok(None);
                }
                keys.push((start, bytes.len() - start, first + row as u64));
            }
            first += reader.part_rows(part) as u64;
        }
        let key = |&(start, len, _): &(usize, usize, u64)| &bytes[start..start + len];
        keys.sort_unstable_by(|left, right| key(left).cmp(key(right)).then(left.2.cmp(&right.2)));
        for entry in &keys {
            writer.push(key(entry), Hit { rid: entry.2, ts: 0 })?;
        }
    }
    let run = writer.finish().encode();
    let mut payload = Vec::with_capacity(HEAD + 4 * columns.len() + run.len());
    payload.extend_from_slice(&covered.to_le_bytes());
    payload.extend_from_slice(&u32::try_from(columns.len()).unwrap_or(u32::MAX).to_le_bytes());
    for &column in columns {
        payload.extend_from_slice(&u32::try_from(column).unwrap_or(u32::MAX).to_le_bytes());
    }
    payload.extend_from_slice(&run);
    Ok(Some(payload))
}

/// The bytes at the front of the payload for a key of `columns` columns.
#[must_use]
pub fn head_bytes(columns: usize) -> usize {
    HEAD + 4 * columns
}

/// The section for the key over `columns`, when the table has one this build can read.
fn section_of<'r>(reader: &'r Reader, columns: &[usize]) -> Option<&'r section::Section> {
    let table = reader.table();
    let id = id(columns);
    table
        .sections()
        .iter()
        .find(|held| held.kind == *section::KEY_RUNS && held.id == id)
        .filter(|held| held.usable(table.generation()))
}

/// Reads the head of a payload: the rows covered, when the columns are `columns`.
fn covered(head: &[u8], columns: &[usize]) -> Option<u64> {
    let covered = u64::from_le_bytes(head.get(..8)?.try_into().ok()?);
    let count = u32::from_le_bytes(head.get(8..HEAD)?.try_into().ok()?) as usize;
    if count != columns.len() {
        return None;
    }
    for (at, &column) in columns.iter().enumerate() {
        let start = HEAD + 4 * at;
        let held = u32::from_le_bytes(head.get(start..start + 4)?.try_into().ok()?);
        if usize::try_from(held).ok()? != column {
            return None;
        }
    }
    Some(covered)
}

/// Whether the table `reader` reads has a run for the key over `columns` that covers enough of its
/// rows to not be written again: three quarters, the share `rudb_native::postings` keeps too.
#[must_use]
pub fn current(reader: &Reader, columns: &[usize]) -> bool {
    let Some(held) = section_of(reader, columns) else { return false };
    let Ok(head) = reader.payload_head(held, head_bytes(columns.len())) else { return false };
    let rows = file_rows(reader);
    covered(&head, columns).is_some_and(|covered| {
        covered <= rows && covered.saturating_mul(4) >= rows.saturating_mul(3)
    })
}

/// The keys of one key of a table's file, read out of its run.
#[derive(Debug)]
pub(crate) struct Sealed {
    /// The rows of the file the run covers, which are its first ones.
    pub(crate) covered: u64,
    keys: Keys,
}

#[derive(Debug)]
enum Keys {
    /// The keys of one `INTEGER` or `BIGINT` column, read out of the run into one array in order,
    /// each with its row's place in the file. A lookup is a search of the array, which is what
    /// the point path's microsecond leaves room for and a read of a block of the run is not.
    Ints(Box<[(i64, u64)]>),
    /// Every other key, as the run, and whether it is of one `VARCHAR` column.
    Run(Run, bool),
}

impl Sealed {
    /// The run for the key over `columns` of the table `reader` reads, when it has one this build
    /// can read.
    pub(crate) fn read(reader: &Reader, columns: &[usize]) -> Option<Self> {
        let held = section_of(reader, columns)?;
        let payload = reader.payload(held).ok()?;
        let covered = covered(&payload, columns)?;
        if covered > file_rows(reader) {
            return None;
        }
        let run = Run::decode(payload.get(head_bytes(columns.len())..)?).ok()?;
        let fields = reader.table().fields();
        let types = columns
            .iter()
            .map(|&at| fields.get(at).map(|field| &field.ty))
            .collect::<Option<Vec<_>>>()?;
        let keys = match types.as_slice() {
            [LogicalType::Integer | LogicalType::BigInt] => {
                let mut keys = Vec::with_capacity(usize::try_from(run.len()).ok()?);
                let mut cursor = run.cursor();
                while cursor.advance() {
                    keys.push((int_of(cursor.key())?, cursor.hit().rid));
                }
                Keys::Ints(keys.into())
            }
            [LogicalType::Varchar] => Keys::Run(run, true),
            _ => Keys::Run(run, false),
        };
        Some(Self { covered, keys })
    }

    pub(crate) fn len(&self) -> usize {
        match &self.keys {
            Keys::Ints(keys) => keys.len(),
            Keys::Run(run, _) => usize::try_from(run.len()).unwrap_or(usize::MAX),
        }
    }

    /// Whether the keys are integers, which a read of a range takes them as.
    pub(crate) fn is_ints(&self) -> bool {
        matches!(self.keys, Keys::Ints(_))
    }

    /// The place in the file of the row holding the key `key`, encoded from `values`, that `kept`
    /// says is still there.
    pub(crate) fn find(
        &self,
        key: Encoded,
        values: &[Value],
        kept: impl Fn(u64) -> bool,
    ) -> Option<u64> {
        match (&self.keys, key) {
            (Keys::Ints(keys), Encoded::Int(key)) => {
                let first = keys.partition_point(|&(held, _)| held < key);
                keys[first..]
                    .iter()
                    .take_while(|&&(held, _)| held == key)
                    .map(|&(_, place)| place)
                    .find(|&place| kept(place))
            }
            (Keys::Run(run, _), Encoded::Bytes) => {
                let mut normal = Vec::with_capacity(16);
                if !normalize(values, &mut normal) {
                    return None;
                }
                run.find(&normal, |hit| kept(hit.rid)).map(|hit| hit.rid)
            }
            _ => None,
        }
    }

    /// The integer keys in order with their places, when the keys are integers.
    pub(crate) fn ints(&self) -> Option<&[(i64, u64)]> {
        match &self.keys {
            Keys::Ints(keys) => Some(keys),
            Keys::Run(..) => None,
        }
    }

    /// The text of every key in order with its place, when the key is one `VARCHAR` column.
    pub(crate) fn texts(&self) -> Vec<(Box<[u8]>, u64)> {
        let Keys::Run(run, true) = &self.keys else { return Vec::new() };
        let mut texts = Vec::with_capacity(self.len());
        let mut cursor = run.cursor();
        while cursor.advance() {
            if let Some(text) = text_of(cursor.key()) {
                texts.push((text, cursor.hit().rid));
            }
        }
        texts
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_int_key_is_the_one_the_index_writes() {
        for key in [i64::MIN, -5, -1, 0, 1, 42, i64::MAX] {
            let mut normal = Vec::new();
            assert!(normalize(&[Value::BigInt(key)], &mut normal));
            assert_eq!(int_key(key).as_slice(), normal.as_slice());
            assert_eq!(int_of(&normal), Some(key));
        }
    }

    #[test]
    fn a_text_key_reads_back() {
        for text in ["", "a", "a\0b", "\0", "ab\0\0c"] {
            let mut normal = Vec::new();
            assert!(normalize(&[Value::Varchar(text.into())], &mut normal));
            assert_eq!(text_of(&normal).as_deref(), Some(text.as_bytes()));
        }
    }

    #[test]
    fn ids_differ_by_column_and_order() {
        assert_ne!(id(&[0]), id(&[1]));
        assert_ne!(id(&[0, 1]), id(&[1, 0]));
        assert_eq!(id(&[3, 4]), id(&[3, 4]));
    }
}
