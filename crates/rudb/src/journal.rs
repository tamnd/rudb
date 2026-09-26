//! The database's log in its first form, `engine-v4/09-the-log.md` and `12-recovery.md`.
//!
//! Until this, a commit was only as durable as the next checkpoint: rows a statement appended sat
//! in memory until `CHECKPOINT`, a close or the last handle going away, and a crash took them. Now
//! the rows an append commits go to one lane of the log in `<database>.wal/` before the statement
//! returns, and the next open replays them.
//!
//! Only appends are logged. Everything else that changes a database's file (an update, a delete, a
//! schema change) checkpoints when it commits instead, which is durable at the price of writing the
//! tables it changed. The row-level Update and Delete records replace that once the hot stripes
//! carry DML. An append too large for a block, or of a type the record does not encode, takes the
//! checkpoint as well.
//!
//! The file's `RUDBWL1` anchor is what keeps a replay from applying a commit twice. Every
//! checkpoint writes the newest commit timestamp into it as the durable cut, and only then are the
//! segments behind the lane's position removed. A crash between the two leaves segments whose
//! commits are all at or below the cut, and replay skips them.
//!
//! A record's payload is logical: the table's schema and name, the row count, and the values a
//! column at a time, each a tag byte and its bytes. It is read back against the table's columns,
//! which cannot have changed between the append and the replay, because a schema change
//! checkpoints and so moves the cut past every append before it.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use rudb_common::{Error, Field, LogicalType, Result, Value};
use rudb_io::{Filesystem, RealFilesystem};
use rudb_native::{LaneStart, LogAnchor};
use rudb_txn::log::{Block, Kind, Lane, Options, SEGMENT_BYTES, SEGMENT_HEADER, replay, segments};
use rudb_vector::{Chunk, Vector};

/// The lane every record goes to, until there are more.
const LANE: u8 = 0;

/// The one payload layout of an Insert record.
const VERSION: u8 = 1;

/// How large a segment is. Smaller than the log's default, because a segment is written out whole
/// when the lane opens it and a database that commits one small insert should not wait for 64 MiB
/// of zeros first.
const SEGMENT: u64 = SEGMENT_BYTES / 4;

/// The most bytes a transaction's records may take before its commit checkpoints instead. A
/// quarter of a segment, so a block always fits one, and large enough that ordinary inserts never
/// meet it; what does is a load, which the checkpoint writes as pages anyway.
const MOST_STAGED: usize = (SEGMENT / 4) as usize;

/// An append read back out of the log, for the table it names.
#[derive(Debug)]
pub(crate) struct Replayed {
    /// The table's schema.
    pub(crate) schema: String,
    /// The table.
    pub(crate) table: String,
    /// The payload after the name, which [`decode_rows`] reads against the table's columns.
    payload: Vec<u8>,
    /// Where the rows start in it.
    rows_at: usize,
}

impl Replayed {
    /// The rows as one chunk, the columns typed as `fields` says.
    ///
    /// # Errors
    ///
    /// If the payload does not read as rows of those columns.
    pub(crate) fn chunk(&self, fields: &[Field]) -> Result<Chunk> {
        decode_rows(&self.payload[self.rows_at..], fields)
    }
}

/// The log of one database file, and the appends the current statement or transaction has staged.
#[derive(Debug)]
pub(crate) struct Journal {
    fs: Arc<dyn Filesystem>,
    dir: PathBuf,
    /// The id every segment header carries.
    database: u64,
    /// The newest commit timestamp handed out, or the cut the file was opened at.
    last: u64,
    /// Opened at the first commit, because opening one makes a segment.
    lane: Option<Lane>,
    /// Insert payloads waiting for the commit.
    staged: Vec<Vec<u8>>,
    staged_bytes: usize,
    /// Whether the commit has to checkpoint rather than log.
    dirty: bool,
    /// Whether the file has an anchor this log is replayed against. Until it does a commit
    /// checkpoints, because a log beside a file without one is taken for another file's.
    anchored: bool,
}

/// The log directory of the database file at `path`.
pub(crate) fn directory(path: &Path) -> PathBuf {
    let mut name = path.as_os_str().to_owned();
    name.push(".wal");
    PathBuf::from(name)
}

impl Journal {
    /// Reads the log of the database file at `path`, whose anchor is `anchor`, and hands back the
    /// journal and every append above the cut in commit order.
    ///
    /// A file without an anchor has never had a commit logged against it, so a log directory
    /// beside it is left over from another file of the same name and is removed, when `writable`.
    ///
    /// # Errors
    ///
    /// If the log cannot be read, or holds segments of another database.
    pub(crate) fn recover(
        path: &Path,
        anchor: Option<&LogAnchor>,
        writable: bool,
    ) -> Result<(Self, Vec<Replayed>)> {
        let fs: Arc<dyn Filesystem> = Arc::new(RealFilesystem::new());
        let dir = directory(path);
        let mut journal = Self {
            fs: Arc::clone(&fs),
            dir: dir.clone(),
            database: anchor.map_or_else(fresh_id, |anchor| anchor.database),
            last: anchor.map_or(0, |anchor| anchor.durable),
            lane: None,
            staged: Vec::new(),
            staged_bytes: 0,
            dirty: false,
            anchored: anchor.is_some(),
        };
        let Some(anchor) = anchor else {
            if writable {
                journal.remove_below(u64::MAX)?;
            }
            return Ok((journal, Vec::new()));
        };
        let read = replay(fs.as_ref(), &dir, LANE, anchor.database)?;
        let mut appends = Vec::new();
        for block in read.blocks {
            let ts = block.commit.commit_ts;
            if !anchor.replays(ts) {
                continue;
            }
            journal.last = journal.last.max(ts);
            for record in block.records {
                if record.header.kind == Kind::Insert {
                    appends.push(read_insert(record.payload)?);
                }
            }
        }
        Ok((journal, appends))
    }

    /// The Insert payload for the rows of `chunks` appended to `schema.table`, whose columns are
    /// `fields`, or `None` when they cannot be logged. Built before the rows go in, so a failed
    /// append has nothing to take back, and [`Self::stage`]d once they are in.
    pub(crate) fn encode(
        &self,
        schema: &str,
        table: &str,
        fields: &[Field],
        chunks: &[Chunk],
    ) -> Option<Vec<u8>> {
        if self.dirty {
            return None;
        }
        encode_insert(schema, table, fields, chunks, MOST_STAGED - self.staged_bytes)
    }

    /// Stages an append for the commit, or marks the commit to checkpoint when there is no payload.
    pub(crate) fn stage(&mut self, payload: Option<Vec<u8>>) {
        if self.dirty {
            return;
        }
        match payload {
            Some(payload) if self.staged_bytes + payload.len() <= MOST_STAGED => {
                self.staged_bytes += payload.len();
                self.staged.push(payload);
            }
            _ => self.dirty(),
        }
    }

    /// Marks the commit to checkpoint.
    pub(crate) fn dirty(&mut self) {
        self.dirty = true;
        self.staged.clear();
        self.staged_bytes = 0;
    }

    /// Drops what was staged, for a transaction that rolled back.
    pub(crate) fn discard(&mut self) {
        self.dirty = false;
        self.staged.clear();
        self.staged_bytes = 0;
    }

    /// Whether the commit has to checkpoint.
    pub(crate) fn needs_checkpoint(&self) -> bool {
        self.dirty || (!self.anchored && !self.staged.is_empty())
    }

    /// Writes what was staged to the lane as one committed block and waits until it is durable.
    ///
    /// # Errors
    ///
    /// If the lane cannot be opened or written. The staged records are dropped either way; a
    /// failed commit is followed by a checkpoint, which the caller asks for.
    pub(crate) fn commit(&mut self) -> Result<()> {
        if self.staged.is_empty() {
            return Ok(());
        }
        let staged = std::mem::take(&mut self.staged);
        self.staged_bytes = 0;
        let ts = self.last + 1;
        let mut block = Block::new(ts, ts, self.last);
        for payload in &staged {
            block.push(Kind::Insert, 0, payload)?;
        }
        let lane = match self.lane.take() {
            Some(lane) => lane,
            None => {
                let options = Options { segment_bytes: SEGMENT, ..Options::new(self.database) };
                Lane::open(Arc::clone(&self.fs), &self.dir, options)?
            }
        };
        let lane = self.lane.insert(lane);
        lane.commit(&block)?;
        self.last = ts;
        Ok(())
    }

    /// The anchor a checkpoint of everything committed so far writes into the file.
    pub(crate) fn anchor(&self) -> LogAnchor {
        let (sequence, offset) = match &self.lane {
            Some(lane) => lane.position(),
            None => (0, SEGMENT_HEADER as u64),
        };
        LogAnchor {
            database: self.database,
            durable: self.last,
            lanes: vec![LaneStart { sequence, offset }],
            voids: Vec::new(),
        }
    }

    /// Recycles what a checkpoint that wrote [`Self::anchor`] made redundant: the staged state
    /// and every segment before the one the lane writes next.
    ///
    /// # Errors
    ///
    /// If a segment cannot be removed.
    pub(crate) fn checkpointed(&mut self) -> Result<()> {
        self.discard();
        self.anchored = true;
        let below = self.lane.as_ref().map_or(u64::MAX, |lane| lane.position().0);
        self.remove_below(below)
    }

    /// Removes the log, for a database whose file was just written whole on the way out.
    ///
    /// # Errors
    ///
    /// If a segment or the directory cannot be removed.
    pub(crate) fn close(&mut self) -> Result<()> {
        self.discard();
        self.lane = None;
        self.remove_below(u64::MAX)?;
        if self.fs.is_dir(&self.dir) && self.fs.read_dir(&self.dir)?.is_empty() {
            std::fs::remove_dir(&self.dir).map_err(|error| Error::io(error.to_string()))?;
        }
        Ok(())
    }

    fn remove_below(&self, below: u64) -> Result<()> {
        for (sequence, path) in segments(self.fs.as_ref(), &self.dir, LANE)? {
            if sequence < below {
                self.fs.remove(&path)?;
            }
        }
        Ok(())
    }
}

/// An id for a database that has never had one, which only has to differ from other databases'.
fn fresh_id() -> u64 {
    use std::hash::{BuildHasher, RandomState};
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |elapsed| elapsed.as_nanos() as u64);
    RandomState::new().hash_one((nanos, std::process::id()))
}

/// An Insert payload, or `None` when a column's type or a value is one the record does not carry.
/// Gives up once the payload passes `most` bytes, which is what keeps a large load from being
/// encoded whole only to be checkpointed instead.
fn encode_insert(
    schema: &str,
    table: &str,
    fields: &[Field],
    chunks: &[Chunk],
    most: usize,
) -> Option<Vec<u8>> {
    if !fields.iter().all(|field| carried(&field.ty)) {
        return None;
    }
    let rows: usize = chunks.iter().map(Chunk::len).sum();
    let mut out = vec![VERSION];
    put_text(&mut out, schema)?;
    put_text(&mut out, table)?;
    out.extend_from_slice(&u16::try_from(fields.len()).ok()?.to_le_bytes());
    out.extend_from_slice(&u32::try_from(rows).ok()?.to_le_bytes());
    for (column, field) in fields.iter().enumerate() {
        for chunk in chunks {
            if chunk.width() != fields.len() {
                return None;
            }
            for row in 0..chunk.len() {
                put(&mut out, &chunk.value_at(row, column), &field.ty)?;
            }
            if out.len() > most {
                return None;
            }
        }
    }
    Some(out)
}

/// The name an Insert payload is for, and where its rows start.
fn read_insert(payload: Vec<u8>) -> Result<Replayed> {
    let mut at = 0;
    if take(&payload, &mut at, 1)?[0] != VERSION {
        return Err(corrupt("an insert record of another layout"));
    }
    let schema = get_text(&payload, &mut at)?;
    let table = get_text(&payload, &mut at)?;
    Ok(Replayed { schema, table, payload, rows_at: at })
}

/// The rows of an Insert payload, from its column count on, as a chunk of `fields`.
fn decode_rows(bytes: &[u8], fields: &[Field]) -> Result<Chunk> {
    let mut at = 0;
    let width = u16::from_le_bytes(array(bytes, &mut at)?) as usize;
    let rows = u32::from_le_bytes(array(bytes, &mut at)?) as usize;
    if width != fields.len() {
        return Err(corrupt(&format!(
            "an insert record of {width} columns for a table of {}",
            fields.len()
        )));
    }
    let mut columns = Vec::with_capacity(width);
    for field in fields {
        let mut values = Vec::with_capacity(rows);
        for _ in 0..rows {
            values.push(get(bytes, &mut at, &field.ty)?);
        }
        columns.push(Vector::from_values(field.ty.clone(), &values)?);
    }
    if at != bytes.len() {
        return Err(corrupt("an insert record with bytes after its rows"));
    }
    Chunk::new(columns)
}

/// Whether values of `ty` go into a record and come back as themselves.
fn carried(ty: &LogicalType) -> bool {
    match ty {
        LogicalType::Boolean
        | LogicalType::TinyInt
        | LogicalType::SmallInt
        | LogicalType::Integer
        | LogicalType::BigInt
        | LogicalType::HugeInt
        | LogicalType::UTinyInt
        | LogicalType::USmallInt
        | LogicalType::UInteger
        | LogicalType::UBigInt
        | LogicalType::UHugeInt
        | LogicalType::Float
        | LogicalType::Double
        | LogicalType::Decimal { .. }
        | LogicalType::Varchar
        | LogicalType::Blob
        | LogicalType::Date
        | LogicalType::Time
        | LogicalType::TimeTz
        | LogicalType::Timestamp
        | LogicalType::TimestampTz
        | LogicalType::Interval => true,
        LogicalType::List(element) => carried(element),
        LogicalType::Struct(fields) => fields.iter().all(|field| carried(&field.ty)),
        LogicalType::Map(key, value) => carried(key) && carried(value),
        _ => false,
    }
}

/// The tag byte of each arm, zero being a null.
mod tag {
    pub(super) const NULL: u8 = 0;
    pub(super) const BOOLEAN: u8 = 1;
    pub(super) const TINYINT: u8 = 2;
    pub(super) const SMALLINT: u8 = 3;
    pub(super) const INTEGER: u8 = 4;
    pub(super) const BIGINT: u8 = 5;
    pub(super) const HUGEINT: u8 = 6;
    pub(super) const UTINYINT: u8 = 7;
    pub(super) const USMALLINT: u8 = 8;
    pub(super) const UINTEGER: u8 = 9;
    pub(super) const UBIGINT: u8 = 10;
    pub(super) const UHUGEINT: u8 = 11;
    pub(super) const FLOAT: u8 = 12;
    pub(super) const DOUBLE: u8 = 13;
    pub(super) const DECIMAL: u8 = 14;
    pub(super) const VARCHAR: u8 = 15;
    pub(super) const BLOB: u8 = 16;
    pub(super) const DATE: u8 = 17;
    pub(super) const TIME: u8 = 18;
    pub(super) const TIME_TZ: u8 = 19;
    pub(super) const TIMESTAMP: u8 = 20;
    pub(super) const TIMESTAMP_TZ: u8 = 21;
    pub(super) const INTERVAL: u8 = 22;
    pub(super) const LIST: u8 = 23;
    pub(super) const STRUCT: u8 = 24;
    pub(super) const MAP: u8 = 25;
}

/// Writes one value of a column of `ty`, or says it cannot.
fn put(out: &mut Vec<u8>, value: &Value, ty: &LogicalType) -> Option<()> {
    let fits = |want: LogicalType| (&want == ty).then_some(());
    match value {
        Value::Null => out.push(tag::NULL),
        Value::Boolean(held) => {
            fits(LogicalType::Boolean)?;
            out.extend_from_slice(&[tag::BOOLEAN, u8::from(*held)]);
        }
        Value::TinyInt(held) => {
            fits(LogicalType::TinyInt)?;
            out.extend_from_slice(&[tag::TINYINT, *held as u8]);
        }
        Value::SmallInt(held) => {
            fits(LogicalType::SmallInt)?;
            fixed(out, tag::SMALLINT, &held.to_le_bytes())
        }
        Value::Integer(held) => {
            fits(LogicalType::Integer)?;
            fixed(out, tag::INTEGER, &held.to_le_bytes())
        }
        Value::BigInt(held) => {
            fits(LogicalType::BigInt)?;
            fixed(out, tag::BIGINT, &held.to_le_bytes())
        }
        Value::HugeInt(held) => {
            fits(LogicalType::HugeInt)?;
            fixed(out, tag::HUGEINT, &held.to_le_bytes())
        }
        Value::UTinyInt(held) => {
            fits(LogicalType::UTinyInt)?;
            out.extend_from_slice(&[tag::UTINYINT, *held]);
        }
        Value::USmallInt(held) => {
            fits(LogicalType::USmallInt)?;
            fixed(out, tag::USMALLINT, &held.to_le_bytes())
        }
        Value::UInteger(held) => {
            fits(LogicalType::UInteger)?;
            fixed(out, tag::UINTEGER, &held.to_le_bytes())
        }
        Value::UBigInt(held) => {
            fits(LogicalType::UBigInt)?;
            fixed(out, tag::UBIGINT, &held.to_le_bytes())
        }
        Value::UHugeInt(held) => {
            fits(LogicalType::UHugeInt)?;
            fixed(out, tag::UHUGEINT, &held.to_le_bytes())
        }
        Value::Float(held) => {
            fits(LogicalType::Float)?;
            fixed(out, tag::FLOAT, &held.to_le_bytes())
        }
        Value::Double(held) => {
            fits(LogicalType::Double)?;
            fixed(out, tag::DOUBLE, &held.to_le_bytes())
        }
        Value::Decimal { unscaled, width, scale } => {
            fits(LogicalType::Decimal { width: *width, scale: *scale })?;
            fixed(out, tag::DECIMAL, &unscaled.to_le_bytes());
        }
        Value::Varchar(text) => {
            fits(LogicalType::Varchar)?;
            out.push(tag::VARCHAR);
            put_bytes(out, text.as_bytes())?;
        }
        Value::Blob(held) => {
            fits(LogicalType::Blob)?;
            out.push(tag::BLOB);
            put_bytes(out, held)?;
        }
        Value::Date(held) => {
            fits(LogicalType::Date)?;
            fixed(out, tag::DATE, &held.to_le_bytes())
        }
        Value::Time(held) => {
            fits(LogicalType::Time)?;
            fixed(out, tag::TIME, &held.to_le_bytes())
        }
        Value::TimeTz(held) => {
            fits(LogicalType::TimeTz)?;
            fixed(out, tag::TIME_TZ, &held.to_le_bytes())
        }
        Value::Timestamp(held) => {
            {
                fits(LogicalType::Timestamp)?;
                fixed(out, tag::TIMESTAMP, &held.to_le_bytes())
            };
        }
        Value::TimestampTz(held) => {
            {
                fits(LogicalType::TimestampTz)?;
                fixed(out, tag::TIMESTAMP_TZ, &held.to_le_bytes())
            };
        }
        Value::Interval { months, days, micros } => {
            fits(LogicalType::Interval)?;
            out.push(tag::INTERVAL);
            out.extend_from_slice(&months.to_le_bytes());
            out.extend_from_slice(&days.to_le_bytes());
            out.extend_from_slice(&micros.to_le_bytes());
        }
        Value::List { values, .. } => {
            let LogicalType::List(element) = ty else { return None };
            out.push(tag::LIST);
            out.extend_from_slice(&u32::try_from(values.len()).ok()?.to_le_bytes());
            for held in values {
                put(out, held, element)?;
            }
        }
        Value::Struct(held) => {
            let LogicalType::Struct(fields) = ty else { return None };
            if held.len() != fields.len() {
                return None;
            }
            out.push(tag::STRUCT);
            for ((_, inner), field) in held.iter().zip(fields) {
                put(out, inner, &field.ty)?;
            }
        }
        Value::Map { entries, .. } => {
            let LogicalType::Map(key, value) = ty else { return None };
            out.push(tag::MAP);
            out.extend_from_slice(&u32::try_from(entries.len()).ok()?.to_le_bytes());
            for (k, v) in entries {
                put(out, k, key)?;
                put(out, v, value)?;
            }
        }
        _ => return None,
    }
    Some(())
}

/// A tag and a fixed width payload.
fn fixed(out: &mut Vec<u8>, tag: u8, payload: &[u8]) {
    out.push(tag);
    out.extend_from_slice(payload);
}

fn put_bytes(out: &mut Vec<u8>, bytes: &[u8]) -> Option<()> {
    out.extend_from_slice(&u32::try_from(bytes.len()).ok()?.to_le_bytes());
    out.extend_from_slice(bytes);
    Some(())
}

fn put_text(out: &mut Vec<u8>, text: &str) -> Option<()> {
    out.extend_from_slice(&u16::try_from(text.len()).ok()?.to_le_bytes());
    out.extend_from_slice(text.as_bytes());
    Some(())
}

/// Reads one value of a column of `ty`.
fn get(bytes: &[u8], at: &mut usize, ty: &LogicalType) -> Result<Value> {
    let tag = take(bytes, at, 1)?[0];
    let value = match tag {
        tag::NULL => Value::Null,
        tag::BOOLEAN => Value::Boolean(take(bytes, at, 1)?[0] != 0),
        tag::TINYINT => Value::TinyInt(take(bytes, at, 1)?[0] as i8),
        tag::SMALLINT => Value::SmallInt(i16::from_le_bytes(array(bytes, at)?)),
        tag::INTEGER => Value::Integer(i32::from_le_bytes(array(bytes, at)?)),
        tag::BIGINT => Value::BigInt(i64::from_le_bytes(array(bytes, at)?)),
        tag::HUGEINT => Value::HugeInt(i128::from_le_bytes(array(bytes, at)?)),
        tag::UTINYINT => Value::UTinyInt(take(bytes, at, 1)?[0]),
        tag::USMALLINT => Value::USmallInt(u16::from_le_bytes(array(bytes, at)?)),
        tag::UINTEGER => Value::UInteger(u32::from_le_bytes(array(bytes, at)?)),
        tag::UBIGINT => Value::UBigInt(u64::from_le_bytes(array(bytes, at)?)),
        tag::UHUGEINT => Value::UHugeInt(u128::from_le_bytes(array(bytes, at)?)),
        tag::FLOAT => Value::Float(f32::from_le_bytes(array(bytes, at)?)),
        tag::DOUBLE => Value::Double(f64::from_le_bytes(array(bytes, at)?)),
        tag::DECIMAL => {
            let LogicalType::Decimal { width, scale } = ty else { return Err(mismatch(tag, ty)) };
            Value::Decimal {
                unscaled: i128::from_le_bytes(array(bytes, at)?),
                width: *width,
                scale: *scale,
            }
        }
        tag::VARCHAR => {
            let len = u32::from_le_bytes(array(bytes, at)?) as usize;
            let text = std::str::from_utf8(take(bytes, at, len)?)
                .map_err(|_| corrupt("a logged string that is not UTF-8"))?;
            Value::Varchar(text.to_string())
        }
        tag::BLOB => {
            let len = u32::from_le_bytes(array(bytes, at)?) as usize;
            Value::Blob(take(bytes, at, len)?.to_vec())
        }
        tag::DATE => Value::Date(i32::from_le_bytes(array(bytes, at)?)),
        tag::TIME => Value::Time(i64::from_le_bytes(array(bytes, at)?)),
        tag::TIME_TZ => Value::TimeTz(i64::from_le_bytes(array(bytes, at)?)),
        tag::TIMESTAMP => Value::Timestamp(i64::from_le_bytes(array(bytes, at)?)),
        tag::TIMESTAMP_TZ => Value::TimestampTz(i64::from_le_bytes(array(bytes, at)?)),
        tag::INTERVAL => Value::Interval {
            months: i32::from_le_bytes(array(bytes, at)?),
            days: i32::from_le_bytes(array(bytes, at)?),
            micros: i64::from_le_bytes(array(bytes, at)?),
        },
        tag::LIST => {
            let LogicalType::List(element) = ty else { return Err(mismatch(tag, ty)) };
            let count = u32::from_le_bytes(array(bytes, at)?) as usize;
            let mut values = Vec::with_capacity(count.min(bytes.len()));
            for _ in 0..count {
                values.push(get(bytes, at, element)?);
            }
            Value::List { element: (**element).clone(), values }
        }
        tag::STRUCT => {
            let LogicalType::Struct(fields) = ty else { return Err(mismatch(tag, ty)) };
            let mut held = Vec::with_capacity(fields.len());
            for field in fields {
                held.push((field.name.clone(), get(bytes, at, &field.ty)?));
            }
            Value::Struct(held)
        }
        tag::MAP => {
            let LogicalType::Map(key, value) = ty else { return Err(mismatch(tag, ty)) };
            let count = u32::from_le_bytes(array(bytes, at)?) as usize;
            let mut entries = Vec::with_capacity(count.min(bytes.len()));
            for _ in 0..count {
                let k = get(bytes, at, key)?;
                entries.push((k, get(bytes, at, value)?));
            }
            Value::map((**key).clone(), (**value).clone(), entries)
        }
        other => return Err(corrupt(&format!("a logged value with tag {other}"))),
    };
    Ok(value)
}

fn get_text(bytes: &[u8], at: &mut usize) -> Result<String> {
    let len = u16::from_le_bytes(array(bytes, at)?) as usize;
    std::str::from_utf8(take(bytes, at, len)?)
        .map(str::to_string)
        .map_err(|_| corrupt("a logged name that is not UTF-8"))
}

fn take<'a>(bytes: &'a [u8], at: &mut usize, len: usize) -> Result<&'a [u8]> {
    let end = at.checked_add(len).filter(|&end| end <= bytes.len());
    let end = end.ok_or_else(|| corrupt("an insert record that ends early"))?;
    let held = &bytes[*at..end];
    *at = end;
    Ok(held)
}

fn array<const N: usize>(bytes: &[u8], at: &mut usize) -> Result<[u8; N]> {
    Ok(take(bytes, at, N)?.try_into().expect("N bytes"))
}

fn corrupt(what: &str) -> Error {
    Error::internal(format!("the log holds {what}"))
}

fn mismatch(tag: u8, ty: &LogicalType) -> Error {
    corrupt(&format!("tag {tag} in a column of {ty}"))
}

#[cfg(test)]
mod tests {
    use rudb_common::{Field, LogicalType, Value};
    use rudb_vector::{Chunk, Vector};

    use super::{decode_rows, encode_insert, read_insert};

    fn column(ty: LogicalType, values: Vec<Value>) -> (Field, Vector) {
        let vector = Vector::from_values(ty.clone(), &values).expect("a vector");
        (Field::new("c", ty), vector)
    }

    #[test]
    fn rows_of_every_carried_type_come_back_as_themselves() {
        let decimal = LogicalType::Decimal { width: 18, scale: 3 };
        let list = LogicalType::List(Box::new(LogicalType::Varchar));
        let pieces = vec![
            column(LogicalType::Boolean, vec![Value::Boolean(true), Value::Null]),
            column(LogicalType::Integer, vec![Value::Integer(-7), Value::Integer(i32::MAX)]),
            column(LogicalType::BigInt, vec![Value::Null, Value::BigInt(1 << 40)]),
            column(LogicalType::Double, vec![Value::Double(0.5), Value::Double(-1e300)]),
            column(
                decimal.clone(),
                vec![Value::Decimal { unscaled: 12_345, width: 18, scale: 3 }, Value::Null],
            ),
            column(
                LogicalType::Varchar,
                vec![Value::Varchar("héllo".into()), Value::Varchar(String::new())],
            ),
            column(LogicalType::Date, vec![Value::Date(19_000), Value::Null]),
            column(LogicalType::Timestamp, vec![Value::Timestamp(1), Value::Timestamp(-1)]),
            column(
                list.clone(),
                vec![
                    Value::List {
                        element: LogicalType::Varchar,
                        values: vec![Value::Varchar("a".into()), Value::Null],
                    },
                    Value::Null,
                ],
            ),
        ];
        let (fields, vectors): (Vec<Field>, Vec<Vector>) = pieces.into_iter().unzip();
        let chunk = Chunk::new(vectors).expect("a chunk");
        let payload =
            encode_insert("main", "items", &fields, &[chunk.clone(), chunk.clone()], usize::MAX)
                .expect("carried");
        let replayed = read_insert(payload).expect("reads");
        assert_eq!((replayed.schema.as_str(), replayed.table.as_str()), ("main", "items"));
        let back = replayed.chunk(&fields).expect("decodes");
        assert_eq!(back.len(), 4);
        for row in 0..4 {
            for col in 0..fields.len() {
                assert_eq!(back.value_at(row, col), chunk.value_at(row % 2, col), "{row} {col}");
            }
        }
        assert!(decode_rows(&[1, 0, 0, 0, 0, 0], &fields).is_err(), "a width that differs");
    }

    #[test]
    fn a_type_the_record_does_not_carry_is_refused() {
        let (field, vector) = column(LogicalType::Uuid, vec![Value::Null]);
        let chunk = Chunk::new(vec![vector]).expect("a chunk");
        assert!(encode_insert("main", "t", &[field], &[chunk], usize::MAX).is_none());
    }
}
