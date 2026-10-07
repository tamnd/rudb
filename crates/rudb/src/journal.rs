//! The database's log in its first form, `engine-v4/09-the-log.md` and `12-recovery.md`.
//!
//! Until this, a commit was only as durable as the next checkpoint: rows a statement appended sat
//! in memory until `CHECKPOINT`, a close or the last handle going away, and a crash took them. Now
//! the rows an append commits go to one lane of the log in `<database>.wal/` before the statement
//! returns, and the next open replays them.
//!
//! Appends, updates and deletes are logged. So are the schema changes that replay can run again
//! from their text alone: a table created without rows, a view, and a drop of either. Their Ddl
//! record is the statement, and replay runs it at its place among the others. Every other schema
//! change checkpoints when it commits instead, which is durable at the price of writing the tables
//! it changed. So does a write too large for a block, or of a type the records do not encode.
//!
//! An Update or Delete record names its rows by where they sit in the table, as runs of row
//! numbers. Those are stable between the statement and the replay, because a table keeps the order
//! its rows went in and every change before this one in the log is replayed first. An Update
//! carries the whole new row, so replay needs nothing of what the row held.
//!
//! The file's `RUDBWL1` anchor is what keeps a replay from applying a commit twice. Every
//! checkpoint writes the newest commit timestamp into it as the durable cut, and only then are the
//! segments behind the lane's position retired, a couple kept to be recycled and the rest removed. A crash between the two leaves segments whose
//! commits are all at or below the cut, and replay skips them.
//!
//! A record's payload is logical: the table's schema and name, the row count, and the values a
//! column at a time, each a tag byte and its bytes. It is read back against the table's columns as
//! replay has them when it reaches the record. Those are the columns the table had when the rows
//! went in, because a change that alters a table's columns checkpoints and so moves the cut past
//! every append before it, and a drop and a create of the same name are replayed in their place.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Weak};
use std::time::Duration;

use rudb_common::{Error, Field, LogicalType, Result, Value};
use rudb_io::{Filesystem, RealFilesystem};
use rudb_native::{LaneStart, LogAnchor};
use rudb_txn::log::{
    Block, Checkpointed, CommitSync, Kind, Lane, Options, Payload, SEGMENT_BYTES, SEGMENT_HEADER,
    replay, segments, spares,
};
use rudb_vector::{Chunk, Data, Selection, StringColumn, VECTOR_SIZE, Validity, Vector};

/// The lane every record goes to, until there are more.
const LANE: u8 = 0;

/// The payload layout records are written in. Layout 1 put every value of an Insert or Update
/// with a tag of its own; layout 2 puts a column a run at a time where it can, and replay reads
/// both.
const VERSION: u8 = 2;

/// How large a segment is. Smaller than the log's default, because a segment is written out whole
/// when the lane opens it and a database that commits one small insert should not wait for 64 MiB
/// of zeros first.
const SEGMENT: u64 = SEGMENT_BYTES / 4;

/// The most bytes a transaction's records may take before its commit checkpoints instead. A
/// quarter of a segment, so a block always fits one, and large enough that ordinary inserts never
/// meet it; what does is a load, which the checkpoint writes as pages anyway.
const MOST_STAGED: usize = (SEGMENT / 4) as usize;

/// How many inserted rows a transaction may log before its commit checkpoints instead: half a
/// stripe, the line between the head and the bulk path in `engine-v4/07-the-head.md` section
/// 7.10. Rows past it are a load, and the checkpoint appends them to the file as pages without
/// logging them first, so a load is written once rather than twice.
const BULK_ROWS: usize = 262_144;

/// How many bytes of blocks `commit_sync = none` lets queue before it writes them out.
const QUEUED: u64 = 1 << 20;

/// How long the log writer waits between two syncs of the commits that did not wait, which is
/// the default `wal_writer_delay` of PostgreSQL.
const WRITER_DELAY: Duration = Duration::from_millis(200);

/// A record staged for the commit: its kind, its payload, and the rows it inserts.
#[derive(Debug)]
pub(crate) struct Record {
    kind: Kind,
    payload: Vec<u8>,
    inserted: usize,
}

/// A change read back out of the log, for the table it names.
#[derive(Debug)]
pub(crate) struct Replayed {
    kind: Kind,
    /// The table's schema.
    pub(crate) schema: String,
    /// The table.
    pub(crate) table: String,
    /// The payload after the name, which [`Self::change`] reads against the table's columns.
    payload: Payload,
    /// Where the rest starts in it.
    rest_at: usize,
    /// The layout it was written in.
    version: u8,
}

/// What a replayed record does to its table.
#[derive(Debug)]
pub(crate) enum Change {
    /// Rows appended at the end. More than one chunk when an insert logged more rows than a chunk
    /// holds.
    Insert(Vec<Chunk>),
    /// The rows in these runs of row numbers taken out.
    Delete(Vec<(u64, u64)>),
    /// The rows in these runs given the rows of the chunks, in order. More than one chunk when an
    /// update wrote more rows than a chunk holds.
    Update(Vec<(u64, u64)>, Vec<Chunk>),
}

impl Change {
    /// Applies the change to `chunks`, which hold the table's rows in order, the columns typed as
    /// `fields` says.
    ///
    /// # Errors
    ///
    /// If a row number is past the last row, or new values do not fit their columns.
    pub(crate) fn apply(self, fields: &[Field], chunks: &mut Vec<Chunk>) -> Result<()> {
        match self {
            Self::Insert(new) => chunks.extend(new),
            Self::Delete(runs) => {
                let mut rows = runs.iter().flat_map(|&(first, len)| first..first + len).peekable();
                let mut offset = 0;
                let mut kept = Vec::with_capacity(chunks.len());
                for chunk in chunks.drain(..) {
                    let end = offset + chunk.len() as u64;
                    let mut gone = Vec::new();
                    while let Some(row) = rows.next_if(|&row| row < end) {
                        gone.push(u32::try_from(row - offset).map_err(|_| corrupt("a row"))?);
                    }
                    offset = end;
                    if gone.is_empty() {
                        kept.push(chunk);
                        continue;
                    }
                    let rest = Selection::from_indices(gone).complement(chunk.len());
                    if !rest.is_empty() {
                        kept.push(chunk.select(&rest)?);
                    }
                }
                *chunks = kept;
                if rows.next().is_some() {
                    return Err(corrupt("a delete record past the table's last row"));
                }
            }
            Self::Update(runs, new) => {
                // Where each new row is, by its place among them.
                let places = new
                    .iter()
                    .enumerate()
                    .flat_map(|(at, chunk)| (0..chunk.len()).map(move |row| (at, row)))
                    .collect::<Vec<_>>();
                let mut rows = runs.iter().flat_map(|&(first, len)| first..first + len).peekable();
                let mut offset = 0;
                let mut next = 0;
                for chunk in chunks.iter_mut() {
                    let end = offset + chunk.len() as u64;
                    let mut hits = Vec::new();
                    while let Some(row) = rows.next_if(|&row| row < end) {
                        hits.push((row - offset) as usize);
                    }
                    offset = end;
                    if hits.is_empty() {
                        continue;
                    }
                    let mut columns = Vec::with_capacity(fields.len());
                    for (column, field) in fields.iter().enumerate() {
                        let mut values = (0..chunk.len())
                            .map(|row| chunk.value_at(row, column))
                            .collect::<Vec<_>>();
                        // row at a time: only the rows an update record names, on a replay of a
                        // table held in memory; a file table's update replays beside the file.
                        for (at, &row) in hits.iter().enumerate() {
                            let (chunk, place) = places[next + at];
                            values[row] = new[chunk].value_at(place, column);
                        }
                        columns.push(Vector::from_values(field.ty.clone(), &values)?);
                    }
                    next += hits.len();
                    *chunk = Chunk::new(columns)?;
                }
                if rows.next().is_some() {
                    return Err(corrupt("an update record past the table's last row"));
                }
            }
        }
        Ok(())
    }
}

impl Replayed {
    /// Whether this is a Ddl record, which [`Self::statement`] reads.
    pub(crate) fn is_statement(&self) -> bool {
        self.kind == Kind::Ddl
    }

    /// The statement a Ddl record carries, which replay runs again, or `None` for any other kind.
    ///
    /// # Errors
    ///
    /// If the payload does not read as one statement's text.
    pub(crate) fn statement(&self) -> Result<Option<String>> {
        if self.kind != Kind::Ddl {
            return Ok(None);
        }
        let mut at = self.rest_at;
        let sql = get_text(&self.payload, &mut at)?;
        if at != self.payload.len() {
            return Err(corrupt("a ddl record with bytes after its statement"));
        }
        Ok(Some(sql))
    }

    /// How many bytes of the log the change took.
    pub(crate) fn size(&self) -> usize {
        self.payload.len()
    }

    /// The change, with rows typed as `fields` says.
    ///
    /// # Errors
    ///
    /// If the payload does not read as a change to a table of those columns.
    pub(crate) fn change(&self, fields: &[Field]) -> Result<Change> {
        let bytes = &self.payload[self.rest_at..];
        match self.kind {
            Kind::Insert => Ok(Change::Insert(decode_rows(bytes, fields, self.version)?.1)),
            Kind::Delete => {
                let mut at = 0;
                let runs = get_runs(bytes, &mut at)?;
                if at != bytes.len() {
                    return Err(corrupt("a delete record with bytes after its rows"));
                }
                Ok(Change::Delete(runs))
            }
            _ => {
                let mut at = 0;
                let runs = get_runs(bytes, &mut at)?;
                let (rows, chunks) = decode_rows(&bytes[at..], fields, self.version)?;
                if runs.iter().map(|run| run.1).sum::<u64>() != rows as u64 {
                    return Err(corrupt("an update record whose rows and row numbers differ"));
                }
                Ok(Change::Update(runs, chunks))
            }
        }
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
    lane: Option<Arc<Lane>>,
    /// Records waiting for the commit.
    staged: Vec<Record>,
    staged_bytes: usize,
    /// The payload bytes committed to the log since the last checkpoint, replayed ones included,
    /// which a commit checks against `checkpoint_threshold`.
    logged: u64,
    /// Whether the commit has to checkpoint rather than log.
    dirty: bool,
    /// Whether the file has an anchor this log is replayed against. Until it does a commit
    /// checkpoints, because a log beside a file without one is taken for another file's.
    anchored: bool,
    /// Whether the log writer runs, see [`wake_writer`].
    writer: Arc<AtomicBool>,
}

/// Starts the log writer when it does not run. It syncs the lane every [`WRITER_DELAY`] while a
/// commit that did not wait is not on stable storage, and stops when all of them are. So a crash
/// loses only the commits of the last moments, as with `synchronous_commit = off` in PostgreSQL,
/// and an idle database has no thread for it.
fn wake_writer(lane: &Arc<Lane>, writer: &Arc<AtomicBool>) {
    if writer.swap(true, Ordering::AcqRel) {
        return;
    }
    let lane = Arc::downgrade(lane);
    let writer = Arc::clone(writer);
    let started = std::thread::Builder::new().name("rudb-log-writer".into()).spawn({
        let writer = Arc::clone(&writer);
        move || write_behind(&lane, &writer)
    });
    if started.is_err() {
        writer.store(false, Ordering::Release);
    }
}

/// The loop of the log writer. A lane that was dropped or failed ends it.
fn write_behind(lane: &Weak<Lane>, writer: &AtomicBool) {
    loop {
        std::thread::sleep(WRITER_DELAY);
        let Some(lane) = lane.upgrade() else { break };
        if lane.behind() {
            if lane.flush().is_err() {
                break;
            }
            continue;
        }
        writer.store(false, Ordering::Release);
        // A commit that queued a block after the look above found the writer still running and
        // did not start another, so this one goes on for it.
        if !lane.behind() || writer.swap(true, Ordering::AcqRel) {
            return;
        }
    }
    writer.store(false, Ordering::Release);
}

/// A block queued in the lane that its commit has not waited for yet.
#[derive(Debug)]
pub(crate) struct Pending {
    lane: Arc<Lane>,
    end: u64,
    sync: CommitSync,
}

impl Pending {
    /// Waits until the block is as far as the commit's `commit_sync` asks, leading the lane's next
    /// write and sync when nobody else is, which carries every block queued behind this one too.
    ///
    /// # Errors
    ///
    /// If the lane failed, now or earlier.
    pub(crate) fn wait(self) -> Result<()> {
        self.lane.settle(self.end, self.sync).map(|_| ())
    }
}

/// The log directory of the database file at `path`.
pub(crate) fn directory(path: &Path) -> PathBuf {
    let mut name = path.as_os_str().to_owned();
    name.push(".wal");
    PathBuf::from(name)
}

impl Journal {
    /// Reads the log of the database file at `path`, whose anchor is `anchor`, and hands back the
    /// journal and every change above the cut in commit order.
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
            logged: 0,
            dirty: false,
            anchored: anchor.is_some(),
            writer: Arc::default(),
        };
        let Some(anchor) = anchor else {
            if writable {
                journal.remove_all()?;
            }
            return Ok((journal, Vec::new()));
        };
        let read = replay(fs.as_ref(), &dir, LANE, anchor.database)?;
        let mut changes = Vec::new();
        for block in read.blocks {
            let ts = block.commit.commit_ts;
            if !anchor.replays(ts) {
                continue;
            }
            journal.last = journal.last.max(ts);
            for record in block.records {
                let kind = record.header.kind;
                if matches!(kind, Kind::Insert | Kind::Update | Kind::Delete | Kind::Ddl) {
                    journal.logged += record.payload.len() as u64;
                    changes.push(read_record(kind, record.payload)?);
                }
            }
        }
        Ok((journal, changes))
    }

    /// The Insert record for the rows of `chunks` appended to `schema.table`, whose columns are
    /// `fields`, or `None` when they cannot be logged or bring the transaction's inserted rows to
    /// [`BULK_ROWS`]. Built before the rows go in, so a failed append has nothing to take back,
    /// and [`Self::stage`]d once they are in.
    pub(crate) fn encode(
        &self,
        schema: &str,
        table: &str,
        fields: &[Field],
        chunks: &[Chunk],
    ) -> Option<Record> {
        let inserted = chunks.iter().map(Chunk::len).sum::<usize>();
        let staged = self.staged.iter().map(|record| record.inserted).sum::<usize>();
        if self.dirty || staged + inserted >= BULK_ROWS {
            return None;
        }
        let mut out = header(schema, table)?;
        put_rows(&mut out, fields, chunks, MOST_STAGED - self.staged_bytes)?;
        Some(Record { kind: Kind::Insert, payload: out, inserted })
    }

    /// [`Self::encode`] for rows handed over as values, each already its column's type or null,
    /// written straight from them rather than first built into a chunk. The record is the one
    /// `encode` would write but for the columns that are not text, which go a value at a time.
    pub(crate) fn encode_values(
        &self,
        schema: &str,
        table: &str,
        fields: &[Field],
        rows: &[Vec<Value>],
    ) -> Option<Record> {
        let staged = self.staged.iter().map(|record| record.inserted).sum::<usize>();
        if self.dirty || staged + rows.len() >= BULK_ROWS {
            return None;
        }
        let mut out = header(schema, table)?;
        put_value_rows(&mut out, fields, rows, MOST_STAGED - self.staged_bytes)?;
        Some(Record { kind: Kind::Insert, payload: out, inserted: rows.len() })
    }

    /// The Delete record for the rows at `rows` of `schema.table`, the row numbers ascending.
    pub(crate) fn encode_delete(&self, schema: &str, table: &str, rows: &[u64]) -> Option<Record> {
        if self.dirty {
            return None;
        }
        let mut out = header(schema, table)?;
        put_runs(&mut out, rows)?;
        (out.len() <= MOST_STAGED - self.staged_bytes).then_some(Record {
            kind: Kind::Delete,
            payload: out,
            inserted: 0,
        })
    }

    /// The Update record that gives the rows at `rows` of `schema.table` the rows of `chunks`, in
    /// order, the row numbers ascending.
    pub(crate) fn encode_update(
        &self,
        schema: &str,
        table: &str,
        fields: &[Field],
        rows: &[u64],
        chunks: &[Chunk],
    ) -> Option<Record> {
        if self.dirty {
            return None;
        }
        let mut out = header(schema, table)?;
        put_runs(&mut out, rows)?;
        put_rows(&mut out, fields, chunks, MOST_STAGED - self.staged_bytes)?;
        Some(Record { kind: Kind::Update, payload: out, inserted: 0 })
    }

    /// The Ddl record for the schema change `sql`, which replay runs again as it is. `None` for a
    /// statement too long for a record's text, which the commit then checkpoints.
    pub(crate) fn encode_ddl(&self, sql: &str) -> Option<Record> {
        if self.dirty {
            return None;
        }
        let mut out = vec![VERSION];
        put_text(&mut out, sql)?;
        (out.len() <= MOST_STAGED - self.staged_bytes).then_some(Record {
            kind: Kind::Ddl,
            payload: out,
            inserted: 0,
        })
    }

    /// A journal that only stages, for a transaction to stage its records in apart from every
    /// other connection's, until its commit hands them to this one with [`Self::absorb`].
    pub(crate) fn shadow(&self) -> Self {
        Self {
            fs: Arc::clone(&self.fs),
            dir: self.dir.clone(),
            database: self.database,
            last: self.last,
            lane: None,
            staged: Vec::new(),
            staged_bytes: 0,
            dirty: false,
            anchored: self.anchored,
            logged: 0,
            writer: Arc::default(),
        }
    }

    /// Takes what a [`Self::shadow`] staged, for the commit of the transaction that staged it.
    pub(crate) fn absorb(&mut self, shadow: Self) {
        if shadow.dirty {
            self.dirty();
            return;
        }
        for record in shadow.staged {
            self.stage(Some(record));
        }
    }

    /// Stages a record for the commit, or marks the commit to checkpoint when there is none.
    pub(crate) fn stage(&mut self, record: Option<Record>) {
        if self.dirty {
            return;
        }
        match record {
            Some(record) if self.staged_bytes + record.payload.len() <= MOST_STAGED => {
                self.staged_bytes += record.payload.len();
                self.staged.push(record);
            }
            _ => self.dirty(),
        }
    }

    /// Whether the commit still logs rather than checkpoints.
    pub(crate) fn logs(&self) -> bool {
        !self.dirty
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

    /// Whether the log has grown to `threshold` bytes since the last checkpoint, after which the
    /// commit that grew it checkpoints so the next open has at most that much to replay.
    pub(crate) fn over(&self, threshold: u64) -> bool {
        self.logged >= threshold
    }

    /// Queues what was staged in the lane as one committed block and hands back what is left to
    /// wait for, which the caller may do after letting go of its locks. `None` when there is
    /// nothing to wait for: nothing was staged, or `sync` is `none`.
    ///
    /// Under [`CommitSync::None`] the blocks queue in the lane and nothing waits, and once a
    /// megabyte of them is queued they are handed to the operating system in one write, so a crash
    /// of the process loses at most that much and the queue does not grow without end.
    ///
    /// # Errors
    ///
    /// If the lane cannot be opened or written. The staged records are dropped either way; a
    /// failed commit is followed by a checkpoint, which the caller asks for.
    pub(crate) fn enqueue(&mut self, sync: CommitSync) -> Result<Option<Pending>> {
        if self.staged.is_empty() {
            return Ok(None);
        }
        let staged = std::mem::take(&mut self.staged);
        self.staged_bytes = 0;
        let ts = self.last + 1;
        let mut block = Block::new(ts, ts, self.last);
        for record in &staged {
            block.push(record.kind, 0, &record.payload)?;
        }
        let lane = match &self.lane {
            Some(lane) => Arc::clone(lane),
            None => {
                let options = Options { segment_bytes: SEGMENT, ..Options::new(self.database) };
                let lane = Arc::new(Lane::open(Arc::clone(&self.fs), &self.dir, options)?);
                self.lane = Some(Arc::clone(&lane));
                lane
            }
        };
        let end = lane.enqueue(&block)?;
        self.last = ts;
        self.logged += staged.iter().map(|record| record.payload.len() as u64).sum::<u64>();
        if sync == CommitSync::None {
            if lane.unwritten() >= QUEUED {
                lane.write_out()?;
            }
            wake_writer(&lane, &self.writer);
            return Ok(None);
        }
        Ok(Some(Pending { lane, end, sync }))
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
    /// and every segment before the one the lane writes next, which the lane keeps a couple of to
    /// make its next segments from and removes the rest of. Then notes the round in the lane as a
    /// Checkpoint record naming the tables it wrote, `09-the-log.md` section 9.4.
    ///
    /// The record goes in after the file took the anchor, so a crash can leave the anchor without
    /// the record and never the other way round. It is a block of its own whose commit timestamp
    /// is the anchor's cut, which replay skips like every other block at or below the cut.
    ///
    /// # Errors
    ///
    /// If a segment cannot be renamed or removed, or the record cannot be written.
    pub(crate) fn checkpointed(&mut self, tables: &[i64], generation: u64) -> Result<()> {
        // Blocks still queued under `commit_sync = none` go out first, so none of them is left to
        // be written into a segment this is about to remove.
        if let Some(lane) = &self.lane {
            lane.write_out()?;
        }
        self.discard();
        self.anchored = true;
        self.logged = 0;
        let Some(lane) = &self.lane else { return self.remove_all() };
        lane.retire(lane.position().0)?;
        let entries = tables
            .iter()
            .filter_map(|&oid| u32::try_from(oid).ok())
            .map(|table| Checkpointed { table, stripe: 0, c_s: self.last, root: 0, generation })
            .collect::<Vec<_>>();
        let mut block = Block::new(0, self.last, self.last);
        block.push(Kind::Checkpoint, 0, &Checkpointed::encode(&entries))?;
        lane.enqueue(&block)?;
        lane.write_out()
    }

    /// Removes the log, for a database whose file was just written whole on the way out.
    ///
    /// # Errors
    ///
    /// If a segment or the directory cannot be removed.
    pub(crate) fn close(&mut self) -> Result<()> {
        self.discard();
        // A lane still shared with a committer outlives this, and its spares thread with it, so
        // that thread is stopped here rather than left to make a spare after the removal.
        if let Some(lane) = self.lane.take() {
            lane.stop();
        }
        self.logged = 0;
        self.remove_all()?;
        if self.fs.is_dir(&self.dir) && self.fs.read_dir(&self.dir)?.is_empty() {
            std::fs::remove_dir(&self.dir).map_err(|error| Error::io(error.to_string()))?;
        }
        Ok(())
    }

    /// Removes every segment and spare of the lane.
    fn remove_all(&self) -> Result<()> {
        for (_, path) in segments(self.fs.as_ref(), &self.dir, LANE)? {
            self.fs.remove(&path)?;
        }
        for path in spares(self.fs.as_ref(), &self.dir, LANE)? {
            self.fs.remove(&path)?;
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

/// The start every payload shares: the layout and the table's name.
fn header(schema: &str, table: &str) -> Option<Vec<u8>> {
    let mut out = vec![VERSION];
    put_text(&mut out, schema)?;
    put_text(&mut out, table)?;
    Some(out)
}

/// Row numbers, ascending, as a count of runs and each run's first row and length.
fn put_runs(out: &mut Vec<u8>, rows: &[u64]) -> Option<()> {
    let mut runs: Vec<(u64, u64)> = Vec::new();
    for &row in rows {
        match runs.last_mut() {
            Some((first, len)) if *first + *len == row => *len += 1,
            Some((first, len)) if *first + *len > row => return None,
            _ => runs.push((row, 1)),
        }
    }
    out.extend_from_slice(&u32::try_from(runs.len()).ok()?.to_le_bytes());
    for (first, len) in runs {
        out.extend_from_slice(&first.to_le_bytes());
        out.extend_from_slice(&len.to_le_bytes());
    }
    Some(())
}

/// The runs [`put_runs`] wrote, checked to ascend without touching.
fn get_runs(bytes: &[u8], at: &mut usize) -> Result<Vec<(u64, u64)>> {
    let count = u32::from_le_bytes(array(bytes, at)?) as usize;
    let mut runs: Vec<(u64, u64)> = Vec::with_capacity(count.min(bytes.len() / 16));
    for _ in 0..count {
        let first = u64::from_le_bytes(array(bytes, at)?);
        let len = u64::from_le_bytes(array(bytes, at)?);
        let after = runs.last().map_or(0, |(first, len)| first + len);
        if len == 0 || first < after || first.checked_add(len).is_none() {
            return Err(corrupt("a record whose row numbers do not ascend"));
        }
        runs.push((first, len));
    }
    Ok(runs)
}

/// The rows of `chunks` a column at a time, or `None` when a column's type or a value is one the
/// records do not carry. Gives up once the payload passes `most` bytes, which is what keeps a large
/// load from being encoded whole only to be checkpointed instead.
///
/// Each column starts with how it is laid out, one of [`mode`]: text and blobs as their lengths
/// and then their bytes, a column every chunk holds flat as its values' bytes, and anything else a
/// value at a time with its tag.
fn put_rows(out: &mut Vec<u8>, fields: &[Field], chunks: &[Chunk], most: usize) -> Option<()> {
    if !fields.iter().all(|field| carried(&field.ty)) {
        return None;
    }
    if chunks.iter().any(|chunk| chunk.width() != fields.len()) {
        return None;
    }
    let rows: usize = chunks.iter().map(Chunk::len).sum();
    u32::try_from(rows).ok()?;
    out.extend_from_slice(&u16::try_from(fields.len()).ok()?.to_le_bytes());
    out.extend_from_slice(&u32::try_from(rows).ok()?.to_le_bytes());
    for (column, field) in fields.iter().enumerate() {
        let vectors: Vec<&Vector> =
            chunks.iter().map(|chunk| chunk.column(column)).collect::<Result<_>>().ok()?;
        if matches!(field.ty, LogicalType::Varchar | LogicalType::Blob) {
            put_text_column(out, &vectors, rows, most)?;
        } else if let Some(layout) = fixed_layout(&vectors) {
            out.push(mode::FIXED);
            out.push(layout);
            put_validity(out, &vectors, rows);
            for vector in &vectors {
                put_fixed(out, vector.data()?, vector.len())?;
            }
        } else {
            out.push(mode::VALUES);
            for (vector, chunk) in vectors.iter().zip(chunks) {
                // row at a time: the fallback for a type with no fixed layout, which an insert
                // record rarely holds; text and fixed width columns are written a run at a time.
                for row in 0..chunk.len() {
                    put(out, &vector.value_at(row), &field.ty)?;
                }
                if out.len() > most {
                    return None;
                }
            }
        }
        if out.len() > most {
            return None;
        }
    }
    Some(())
}

/// [`put_rows`] for rows as values, every one of them null or of its column's type.
/// The bytes of a text or blob value, `Some(None)` for a null, and `None` for anything else.
fn text_of(value: &Value) -> Option<Option<&[u8]>> {
    match value {
        Value::Varchar(text) => Some(Some(text.as_bytes())),
        Value::Blob(bytes) => Some(Some(bytes.as_slice())),
        Value::Null => Some(None),
        _ => None,
    }
}

fn put_value_rows(
    out: &mut Vec<u8>,
    fields: &[Field],
    rows: &[Vec<Value>],
    most: usize,
) -> Option<()> {
    if !fields.iter().all(|field| carried(&field.ty)) {
        return None;
    }
    if rows.iter().any(|row| row.len() != fields.len()) {
        return None;
    }
    out.extend_from_slice(&u16::try_from(fields.len()).ok()?.to_le_bytes());
    out.extend_from_slice(&u32::try_from(rows.len()).ok()?.to_le_bytes());
    for (column, field) in fields.iter().enumerate() {
        if matches!(field.ty, LogicalType::Varchar | LogicalType::Blob) {
            let mut bytes = 0;
            let mut nulls = false;
            for row in rows {
                match text_of(&row[column])? {
                    Some(text) => bytes += text.len(),
                    None => nulls = true,
                }
            }
            if out.len() + bytes > most {
                return None;
            }
            out.push(mode::TEXT);
            if nulls {
                out.push(1);
                let start = out.len();
                out.resize(start + rows.len().div_ceil(8), 0);
                for (at, row) in rows.iter().enumerate() {
                    if !row[column].is_null() {
                        out[start + at / 8] |= 1 << (at % 8);
                    }
                }
            } else {
                out.push(0);
            }
            out.reserve(4 * rows.len() + bytes);
            for row in rows {
                let len = text_of(&row[column])?.map_or(0, <[u8]>::len);
                out.extend_from_slice(&u32::try_from(len).ok()?.to_le_bytes());
            }
            for row in rows {
                if let Some(text) = text_of(&row[column])? {
                    out.extend_from_slice(text);
                }
            }
        } else {
            out.push(mode::VALUES);
            for row in rows {
                put(out, &row[column], &field.ty)?;
            }
        }
        if out.len() > most {
            return None;
        }
    }
    Some(())
}

/// How a column of an Insert or Update payload is laid out, from layout 2 on.
mod mode {
    /// A value at a time, each with its tag, which every carried type can take.
    pub(super) const VALUES: u8 = 0;
    /// The nulls, then each value's bytes at the width of its layout.
    pub(super) const FIXED: u8 = 1;
    /// The nulls, then each value's length as four bytes, then all their bytes.
    pub(super) const TEXT: u8 = 2;
}

/// The nulls of a column: a zero byte when there are none, or a one and a bit a row, set meaning
/// valid, first row in the lowest bit.
fn put_validity(out: &mut Vec<u8>, vectors: &[&Vector], rows: usize) {
    if vectors.iter().all(|vector| vector.never_null()) {
        out.push(0);
        return;
    }
    out.push(1);
    let start = out.len();
    out.resize(start + rows.div_ceil(8), 0);
    let mut row = 0;
    for vector in vectors {
        for at in 0..vector.len() {
            if !vector.is_null_at(at) {
                out[start + row / 8] |= 1 << (row % 8);
            }
            row += 1;
        }
    }
}

fn get_validity(bytes: &[u8], at: &mut usize, cuts: &[(usize, usize)]) -> Result<Vec<Validity>> {
    let rows = cuts.last().map_or(0, |&(start, len)| start + len);
    match take(bytes, at, 1)?[0] {
        0 => Ok(vec![Validity::AllValid; cuts.len()]),
        1 => {
            let bitmap = take(bytes, at, rows.div_ceil(8))?;
            Ok(cuts
                .iter()
                .map(|&(start, len)| {
                    Validity::from_bytes(len, &bitmap[start / 8..(start + len).div_ceil(8)])
                })
                .collect())
        }
        _ => Err(corrupt("a column whose nulls are neither absent nor a bitmap")),
    }
}

/// A text or blob column as [`mode::TEXT`].
fn put_text_column(out: &mut Vec<u8>, vectors: &[&Vector], rows: usize, most: usize) -> Option<()> {
    let mut texts = Vec::with_capacity(rows);
    for vector in vectors {
        for at in 0..vector.len() {
            texts.push(vector.try_bytes_at(at).ok()?);
        }
    }
    let bytes: usize = texts.iter().map(|text| text.map_or(0, <[u8]>::len)).sum();
    if out.len() + bytes > most {
        return None;
    }
    out.push(mode::TEXT);
    if texts.iter().all(Option::is_some) {
        out.push(0);
    } else {
        out.push(1);
        let start = out.len();
        out.resize(start + rows.div_ceil(8), 0);
        for (row, text) in texts.iter().enumerate() {
            if text.is_some() {
                out[start + row / 8] |= 1 << (row % 8);
            }
        }
    }
    out.reserve(4 * rows + bytes);
    for text in &texts {
        out.extend_from_slice(&u32::try_from(text.map_or(0, <[u8]>::len)).ok()?.to_le_bytes());
    }
    for text in texts.into_iter().flatten() {
        out.extend_from_slice(text);
    }
    Some(())
}

/// The layout byte every one of `vectors` shares when each is flat over data of a fixed width.
fn fixed_layout(vectors: &[&Vector]) -> Option<u8> {
    let mut shared = None;
    for vector in vectors {
        let data = vector.data()?;
        if data.len() < vector.len() {
            return None;
        }
        let layout = match data {
            Data::Bool(_) => 1,
            Data::Int8(_) => 2,
            Data::Int16(_) => 3,
            Data::Int32(_) => 4,
            Data::Int64(_) => 5,
            Data::Int128(_) => 6,
            Data::UInt8(_) => 7,
            Data::UInt16(_) => 8,
            Data::UInt32(_) => 9,
            Data::UInt64(_) => 10,
            Data::UInt128(_) => 11,
            Data::Float32(_) => 12,
            Data::Float64(_) => 13,
            _ => return None,
        };
        if shared.is_some_and(|shared| shared != layout) {
            return None;
        }
        shared = Some(layout);
    }
    shared
}

/// The first `len` values of `data` as their little endian bytes.
fn put_fixed(out: &mut Vec<u8>, data: &Data, len: usize) -> Option<()> {
    macro_rules! bytes {
        ($held:expr) => {
            for value in &$held.as_slice()[..len] {
                out.extend_from_slice(&value.to_le_bytes());
            }
        };
    }
    match data {
        Data::Bool(held) => out.extend(held.as_slice()[..len].iter().map(|&value| u8::from(value))),
        Data::Int8(held) => bytes!(held),
        Data::Int16(held) => bytes!(held),
        Data::Int32(held) => bytes!(held),
        Data::Int64(held) => bytes!(held),
        Data::Int128(held) => bytes!(held),
        Data::UInt8(held) => bytes!(held),
        Data::UInt16(held) => bytes!(held),
        Data::UInt32(held) => bytes!(held),
        Data::UInt64(held) => bytes!(held),
        Data::UInt128(held) => bytes!(held),
        Data::Float32(held) => bytes!(held),
        Data::Float64(held) => bytes!(held),
        _ => return None,
    }
    Some(())
}

/// `rows` values of layout `layout` read back as data.
fn get_fixed(bytes: &[u8], at: &mut usize, layout: u8, rows: usize) -> Result<Data> {
    macro_rules! read {
        ($variant:ident, $ty:ty) => {{
            const WIDTH: usize = std::mem::size_of::<$ty>();
            let held = take(
                bytes,
                at,
                rows.checked_mul(WIDTH).ok_or_else(|| corrupt("a column too long"))?,
            )?;
            Data::$variant(
                held.chunks_exact(WIDTH)
                    .map(|one| <$ty>::from_le_bytes(one.try_into().expect("WIDTH bytes")))
                    .collect(),
            )
        }};
    }
    Ok(match layout {
        1 => Data::Bool(take(bytes, at, rows)?.iter().map(|&byte| byte != 0).collect()),
        2 => read!(Int8, i8),
        3 => read!(Int16, i16),
        4 => read!(Int32, i32),
        5 => read!(Int64, i64),
        6 => read!(Int128, i128),
        7 => read!(UInt8, u8),
        8 => read!(UInt16, u16),
        9 => read!(UInt32, u32),
        10 => read!(UInt64, u64),
        11 => read!(UInt128, u128),
        12 => read!(Float32, f32),
        13 => read!(Float64, f64),
        _ => return Err(corrupt(&format!("a column of layout {layout}"))),
    })
}

/// A column written as [`mode::TEXT`], as vectors of `field`'s type cut at `cuts`.
fn get_text_column(
    bytes: &[u8],
    at: &mut usize,
    field: &Field,
    cuts: &[(usize, usize)],
) -> Result<Vec<Vector>> {
    let rows = cuts.last().map_or(0, |&(start, len)| start + len);
    let validity = get_validity(bytes, at, cuts)?;
    let lengths =
        take(bytes, at, rows.checked_mul(4).ok_or_else(|| corrupt("a column too long"))?)?;
    let total: usize = lengths
        .chunks_exact(4)
        .map(|one| u32::from_le_bytes(one.try_into().expect("four bytes")) as usize)
        .sum();
    let body = take(bytes, at, total)?;
    // Checked as one run, and each string is then a slice of it that has to start and end on a
    // character. Checking a string at a time spent more on the calls than on the bytes.
    let text = if field.ty == LogicalType::Varchar {
        Some(std::str::from_utf8(body).map_err(|_| corrupt("logged text that is not UTF-8"))?)
    } else {
        None
    };
    let mut from = 0;
    let mut pieces = Vec::with_capacity(cuts.len());
    for (&(start, len), validity) in cuts.iter().zip(validity) {
        let mut column = StringColumn::with_capacity(len);
        for one in lengths[start * 4..(start + len) * 4].chunks_exact(4) {
            let end = from + u32::from_le_bytes(one.try_into().expect("four bytes")) as usize;
            match text {
                Some(text) => {
                    let value = text
                        .get(from..end)
                        .ok_or_else(|| corrupt("logged text cut inside a character"))?;
                    column.push(value);
                }
                None => {
                    column.push_bytes(&body[from..end]);
                }
            }
            from = end;
        }
        pieces.push(Vector::flat(field.ty.clone(), Data::Varlen(column))?.with_validity(validity));
    }
    Ok(pieces)
}

/// The name a payload is for, and where the rest of it starts. A Ddl record names no table, and its
/// statement starts right after the layout byte.
fn read_record(kind: Kind, payload: Payload) -> Result<Replayed> {
    let mut at = 0;
    let version = take(&payload, &mut at, 1)?[0];
    if !(1..=VERSION).contains(&version) {
        return Err(corrupt("a record of another layout"));
    }
    if kind == Kind::Ddl {
        return Ok(Replayed {
            kind,
            schema: String::new(),
            table: String::new(),
            payload,
            rest_at: at,
            version,
        });
    }
    let schema = get_text(&payload, &mut at)?;
    let table = get_text(&payload, &mut at)?;
    Ok(Replayed { kind, schema, table, payload, rest_at: at, version })
}

/// The rows of a payload, from its column count on, as chunks of `fields`, and how many there
/// are. One record holds all the rows of a statement, which can be more than a chunk, so they are
/// cut at the chunk size.
///
/// A payload of layout 1 has every column a value at a time; one of layout 2 says how each column
/// is laid out first.
fn decode_rows(bytes: &[u8], fields: &[Field], version: u8) -> Result<(usize, Vec<Chunk>)> {
    let (columns, rows) = decode_columns(bytes, fields, version)?;
    let mut columns = columns.into_iter().map(Vec::into_iter).collect::<Vec<_>>();
    let mut chunks = Vec::with_capacity(rows.div_ceil(VECTOR_SIZE).max(1));
    for start in (0..rows.max(1)).step_by(VECTOR_SIZE) {
        let len = VECTOR_SIZE.min(rows - start);
        let piece = columns
            .iter_mut()
            .map(|pieces| pieces.next().ok_or_else(|| corrupt("a column cut short")))
            .collect::<Result<Vec<_>>>()?;
        chunks.push(Chunk::with_rows(piece, len)?);
    }
    Ok((rows, chunks))
}

/// The rows of a payload as the pieces of each column, a chunk's worth a piece and at least one
/// piece, and how many rows there are, which may be more than a chunk.
///
/// A column laid out flat or as text is read straight into its pieces. Read whole and then cut, as
/// it was, every value was copied twice, and on a replay of fifty million rows the cutting was as
/// much work as the reading.
fn decode_columns(
    bytes: &[u8],
    fields: &[Field],
    version: u8,
) -> Result<(Vec<Vec<Vector>>, usize)> {
    let mut at = 0;
    let width = u16::from_le_bytes(array(bytes, &mut at)?) as usize;
    let rows = u32::from_le_bytes(array(bytes, &mut at)?) as usize;
    if width != fields.len() {
        return Err(corrupt(&format!(
            "an insert record of {width} columns for a table of {}",
            fields.len()
        )));
    }
    // Where each piece starts and how long it is. A chunk is a whole number of bytes of a null
    // bitmap long, so every piece's bits start on a byte.
    let cuts = (0..rows.max(1))
        .step_by(VECTOR_SIZE)
        .map(|start| (start, VECTOR_SIZE.min(rows - start)))
        .collect::<Vec<_>>();
    let mut columns = Vec::with_capacity(width);
    for field in fields {
        let layout = if version == 1 { mode::VALUES } else { take(bytes, &mut at, 1)?[0] };
        let pieces = match layout {
            mode::VALUES => {
                let mut values = Vec::with_capacity(rows.min(bytes.len()));
                for _ in 0..rows {
                    values.push(get(bytes, &mut at, &field.ty)?);
                }
                let column = Vector::from_values(field.ty.clone(), &values)?;
                if cuts.len() == 1 {
                    vec![column]
                } else {
                    cuts.iter()
                        .map(|&(start, len)| column.slice(start, len))
                        .collect::<Result<Vec<_>>>()?
                }
            }
            mode::FIXED => {
                let layout = take(bytes, &mut at, 1)?[0];
                let validity = get_validity(bytes, &mut at, &cuts)?;
                let mut pieces = Vec::with_capacity(cuts.len());
                for (&(_, len), validity) in cuts.iter().zip(validity) {
                    let data = get_fixed(bytes, &mut at, layout, len)?;
                    pieces.push(Vector::flat(field.ty.clone(), data)?.with_validity(validity));
                }
                pieces
            }
            mode::TEXT => get_text_column(bytes, &mut at, field, &cuts)?,
            other => return Err(corrupt(&format!("a column laid out as {other}"))),
        };
        columns.push(pieces);
    }
    if at != bytes.len() {
        return Err(corrupt("an insert record with bytes after its rows"));
    }
    Ok((columns, rows))
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
        LogicalType::Union(members) => members.iter().all(|member| carried(&member.ty)),
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
    pub(super) const TIMESTAMP_S: u8 = 26;
    pub(super) const TIMESTAMP_MS: u8 = 27;
    pub(super) const TIMESTAMP_NS: u8 = 28;
    pub(super) const UUID: u8 = 29;
    pub(super) const UNION: u8 = 30;
    pub(super) const TIME_NS: u8 = 31;
    pub(super) const TIMESTAMP_TZ_NS: u8 = 32;
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
        Value::Uuid(held) => {
            fits(LogicalType::Uuid)?;
            fixed(out, tag::UUID, &held.to_le_bytes())
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
        Value::TimestampS(held) => {
            fits(LogicalType::TimestampS)?;
            fixed(out, tag::TIMESTAMP_S, &held.to_le_bytes());
        }
        Value::TimestampMs(held) => {
            fits(LogicalType::TimestampMs)?;
            fixed(out, tag::TIMESTAMP_MS, &held.to_le_bytes());
        }
        Value::TimestampNs(held) => {
            fits(LogicalType::TimestampNs)?;
            fixed(out, tag::TIMESTAMP_NS, &held.to_le_bytes());
        }
        Value::TimeNs(held) => {
            fits(LogicalType::TimeNs)?;
            fixed(out, tag::TIME_NS, &held.to_le_bytes());
        }
        Value::TimestampTzNs(held) => {
            fits(LogicalType::TimestampTzNs)?;
            fixed(out, tag::TIMESTAMP_TZ_NS, &held.to_le_bytes());
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
        Value::Union { tag, value, .. } => {
            let LogicalType::Union(members) = ty else { return None };
            let member = members.get(usize::from(*tag))?;
            out.push(tag::UNION);
            out.push(*tag);
            put(out, value, &member.ty)?;
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
        tag::UUID => Value::Uuid(i128::from_le_bytes(array(bytes, at)?)),
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
        tag::TIMESTAMP_S => Value::TimestampS(i64::from_le_bytes(array(bytes, at)?)),
        tag::TIMESTAMP_MS => Value::TimestampMs(i64::from_le_bytes(array(bytes, at)?)),
        tag::TIMESTAMP_NS => Value::TimestampNs(i64::from_le_bytes(array(bytes, at)?)),
        tag::TIME_NS => Value::TimeNs(i64::from_le_bytes(array(bytes, at)?)),
        tag::TIMESTAMP_TZ_NS => Value::TimestampTzNs(i64::from_le_bytes(array(bytes, at)?)),
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
        tag::UNION => {
            let LogicalType::Union(members) = ty else { return Err(mismatch(tag, ty)) };
            let held = take(bytes, at, 1)?[0];
            let member = members
                .get(usize::from(held))
                .ok_or_else(|| corrupt(&format!("a logged union with tag {held}")))?;
            let value = Box::new(get(bytes, at, &member.ty)?);
            Value::Union { members: members.clone(), tag: held, value }
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
    use rudb_txn::log::Kind;
    use rudb_vector::{Chunk, Vector};

    use super::{
        Change, decode_rows, header, put, put_rows, put_runs, put_text, put_value_rows, read_record,
    };

    #[test]
    fn the_log_writer_syncs_the_commits_that_did_not_wait_and_then_stops() {
        use std::sync::Arc;
        use std::sync::atomic::{AtomicBool, Ordering};

        use rudb_io::SimFilesystem;
        use rudb_txn::log::{Block, Lane, Options};

        let fs = Arc::new(SimFilesystem::new());
        let lane = Lane::open(fs, std::path::Path::new("/log"), Options::new(1)).unwrap();
        let lane = Arc::new(lane);
        let writer = Arc::new(AtomicBool::new(false));
        let mut block = Block::new(1, 1, 0);
        block.push(Kind::Insert, 0, b"row").unwrap();
        lane.enqueue(&block).unwrap();
        assert!(lane.behind());
        super::wake_writer(&lane, &writer);
        assert!(writer.load(Ordering::Acquire));
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        while (lane.behind() || writer.load(Ordering::Acquire))
            && std::time::Instant::now() < deadline
        {
            std::thread::sleep(super::WRITER_DELAY / 4);
        }
        assert!(!lane.behind());
        assert!(!writer.load(Ordering::Acquire));
    }

    fn insert(fields: &[Field], chunks: &[Chunk]) -> Option<Vec<u8>> {
        let mut out = header("main", "items")?;
        put_rows(&mut out, fields, chunks, usize::MAX)?;
        Some(out)
    }

    fn column(ty: LogicalType, values: Vec<Value>) -> (Field, Vector) {
        let vector = Vector::from_values(ty.clone(), &values).expect("a vector");
        (Field::new("c", ty), vector)
    }

    #[test]
    fn rows_of_every_carried_type_come_back_as_themselves() {
        let decimal = LogicalType::Decimal { width: 18, scale: 3 };
        let list = LogicalType::List(Box::new(LogicalType::Varchar));
        let members =
            vec![Field::new("a", LogicalType::Integer), Field::new("b", LogicalType::Varchar)];
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
            column(
                LogicalType::Union(members.clone()),
                vec![
                    Value::Union {
                        members: members.clone(),
                        tag: 1,
                        value: Box::new(Value::Varchar("x".into())),
                    },
                    Value::Union { members: members.clone(), tag: 0, value: Box::new(Value::Null) },
                ],
            ),
        ];
        let (fields, vectors): (Vec<Field>, Vec<Vector>) = pieces.into_iter().unzip();
        let chunk = Chunk::new(vectors).expect("a chunk");
        let payload = insert(&fields, &[chunk.clone(), chunk.clone()]).expect("carried");
        let replayed = read_record(Kind::Insert, payload.into()).expect("reads");
        assert_eq!((replayed.schema.as_str(), replayed.table.as_str()), ("main", "items"));
        let Change::Insert(back) = replayed.change(&fields).expect("decodes") else {
            panic!("an insert")
        };
        let [back] = <[Chunk; 1]>::try_from(back).expect("one chunk");
        assert_eq!(back.len(), 4);
        for row in 0..4 {
            for col in 0..fields.len() {
                assert_eq!(back.value_at(row, col), chunk.value_at(row % 2, col), "{row} {col}");
            }
        }
        assert!(decode_rows(&[1, 0, 0, 0, 0, 0], &fields, 2).is_err(), "a width that differs");
    }

    fn replayed_insert(fields: &[Field], payload: Vec<u8>) -> Chunk {
        let replayed = read_record(Kind::Insert, payload.into()).expect("reads");
        let Change::Insert(back) = replayed.change(fields).expect("decodes") else {
            panic!("an insert")
        };
        <[Chunk; 1]>::try_from(back).expect("one chunk").into_iter().next().expect("one")
    }

    #[test]
    fn rows_written_from_their_values_replay_as_the_rows_of_a_chunk_do() {
        let decimal = LogicalType::Decimal { width: 18, scale: 3 };
        let fields = [
            Field::new("k", LogicalType::Varchar),
            Field::new("n", LogicalType::BigInt),
            Field::new("b", LogicalType::Blob),
            Field::new("d", decimal.clone()),
            Field::new("t", LogicalType::Varchar),
        ];
        let rows = (0..11_i64)
            .map(|at| {
                vec![
                    Value::Varchar(format!("user{at}")),
                    if at % 3 == 0 { Value::Null } else { Value::BigInt(at << 33) },
                    Value::Blob(vec![at as u8; at as usize]),
                    Value::Decimal { unscaled: i128::from(at) * 1_001, width: 18, scale: 3 },
                    if at % 4 == 1 {
                        Value::Null
                    } else {
                        Value::Varchar("é".repeat(at as usize))
                    },
                ]
            })
            .collect::<Vec<_>>();
        let vectors = fields
            .iter()
            .enumerate()
            .map(|(at, field)| {
                let values = rows.iter().map(|row| row[at].clone()).collect::<Vec<_>>();
                Vector::from_values(field.ty.clone(), &values).expect("a vector")
            })
            .collect::<Vec<_>>();
        let chunk = Chunk::new(vectors).expect("a chunk");
        let mut payload = header("main", "items").expect("a header");
        put_value_rows(&mut payload, &fields, &rows, usize::MAX).expect("carried");
        let back = replayed_insert(&fields, payload);
        let whole = replayed_insert(&fields, insert(&fields, &[chunk]).expect("carried"));
        assert_eq!(back.len(), rows.len());
        for (at, row) in rows.iter().enumerate() {
            for (column, value) in row.iter().enumerate() {
                assert_eq!(&back.value_at(at, column), value, "{at} {column}");
                assert_eq!(back.value_at(at, column), whole.value_at(at, column));
            }
        }
        let mut out = Vec::new();
        let mut wrong = rows.clone();
        wrong[4][1] = Value::Integer(1);
        assert!(put_value_rows(&mut out, &fields, &wrong, usize::MAX).is_none(), "not its type");
        wrong[4][1] = Value::BigInt(1);
        wrong[2][4] = Value::Integer(1);
        assert!(put_value_rows(&mut out, &fields, &wrong, usize::MAX).is_none(), "not text");
        wrong[2][4] = Value::Null;
        wrong[3].pop();
        assert!(put_value_rows(&mut out, &fields, &wrong, usize::MAX).is_none(), "too short");
        let mut out = Vec::new();
        assert!(put_value_rows(&mut out, &fields, &rows, 40).is_none(), "too big");
    }

    #[test]
    fn an_insert_of_more_rows_than_a_chunk_holds_comes_back_cut_into_chunks() {
        let fields = vec![Field::new("n", LogicalType::BigInt)];
        let chunks = (0..3_i64)
            .map(|at| {
                let values =
                    (0..8_000).map(|row| Value::BigInt(at * 8_000 + row)).collect::<Vec<_>>();
                let column = Vector::from_values(LogicalType::BigInt, &values).expect("a column");
                Chunk::new(vec![column]).expect("a chunk")
            })
            .collect::<Vec<_>>();
        let payload = insert(&fields, &chunks).expect("carried");
        let replayed = read_record(Kind::Insert, payload.into()).expect("reads");
        let Change::Insert(back) = replayed.change(&fields).expect("decodes") else {
            panic!("an insert")
        };
        assert_eq!(back.iter().map(Chunk::len).collect::<Vec<_>>(), [8_192, 8_192, 7_616]);
        let mut next = 0;
        for chunk in &back {
            for row in 0..chunk.len() {
                assert_eq!(chunk.value_at(row, 0), Value::BigInt(next));
                next += 1;
            }
        }
    }

    #[test]
    fn a_record_of_the_first_layout_still_replays() {
        let fields =
            [Field::new("id", LogicalType::BigInt), Field::new("name", LogicalType::Varchar)];
        let rows = [[Value::BigInt(7), Value::Varchar("seven".into())], [Value::Null, Value::Null]];
        let mut payload = vec![1];
        put_text(&mut payload, "main").expect("a name");
        put_text(&mut payload, "items").expect("a name");
        payload.extend_from_slice(&2_u16.to_le_bytes());
        payload.extend_from_slice(&2_u32.to_le_bytes());
        for (column, field) in fields.iter().enumerate() {
            for row in &rows {
                put(&mut payload, &row[column], &field.ty).expect("carried");
            }
        }
        let back = replayed_insert(&fields, payload);
        for (at, row) in rows.iter().enumerate() {
            assert_eq!(back.value_at(at, 0), row[0]);
            assert_eq!(back.value_at(at, 1), row[1]);
        }
    }

    #[test]
    fn columns_that_are_not_flat_or_differ_between_chunks_go_a_value_at_a_time() {
        let fields = [
            Field::new("n", LogicalType::Integer),
            Field::new("b", LogicalType::Blob),
            Field::new("s", LogicalType::Varchar),
        ];
        let first = Chunk::new(vec![
            Vector::constant(LogicalType::Integer, Value::Integer(5), 3),
            Vector::from_values(
                LogicalType::Blob,
                &[Value::Blob(vec![0xFF, 0]), Value::Null, Value::Blob(vec![])],
            )
            .expect("blobs"),
            Vector::constant(LogicalType::Varchar, Value::Varchar("same".into()), 3),
        ])
        .expect("a chunk");
        let second = Chunk::new(vec![
            Vector::from_values(LogicalType::Integer, &[Value::Integer(-1)]).expect("ints"),
            Vector::from_values(LogicalType::Blob, &[Value::Blob(b"x".to_vec())]).expect("blobs"),
            Vector::from_values(LogicalType::Varchar, &[Value::Null]).expect("text"),
        ])
        .expect("a chunk");
        let payload = insert(&fields, &[first.clone(), second.clone()]).expect("carried");
        let back = replayed_insert(&fields, payload);
        assert_eq!(back.len(), 4);
        for col in 0..fields.len() {
            for row in 0..3 {
                assert_eq!(back.value_at(row, col), first.value_at(row, col), "{row} {col}");
            }
            assert_eq!(back.value_at(3, col), second.value_at(0, col), "3 {col}");
        }
    }

    #[test]
    fn a_type_the_record_does_not_carry_is_refused() {
        let (field, vector) = column(LogicalType::Uuid, vec![Value::Null]);
        let chunk = Chunk::new(vec![vector]).expect("a chunk");
        assert!(insert(&[field], &[chunk]).is_none());
    }

    #[test]
    fn row_numbers_go_in_as_runs() {
        let (field, vector) = column(LogicalType::Integer, vec![Value::Integer(9); 5]);
        let fields = [field];
        let chunk = Chunk::new(vec![vector]).expect("a chunk");
        let mut out = header("main", "t").expect("a name");
        put_runs(&mut out, &[0, 1, 2, 7, 9, 10]).expect("ascending");
        let replayed = read_record(Kind::Delete, out.clone().into()).expect("reads");
        let Change::Delete(runs) = replayed.change(&fields).expect("decodes") else {
            panic!("a delete")
        };
        assert_eq!(runs, vec![(0, 3), (7, 1), (9, 2)]);
        let mut update = header("main", "t").expect("a name");
        put_runs(&mut update, &[3, 4, 5, 6, 20]).expect("ascending");
        put_rows(&mut update, &fields, std::slice::from_ref(&chunk), usize::MAX).expect("rows");
        let replayed = read_record(Kind::Update, update.into()).expect("reads");
        let Change::Update(runs, rows) = replayed.change(&fields).expect("decodes") else {
            panic!("an update")
        };
        assert_eq!((runs, rows.iter().map(Chunk::len).sum::<usize>()), (vec![(3, 4), (20, 1)], 5));
        assert!(put_runs(&mut Vec::new(), &[4, 2]).is_none(), "row numbers that fall");
        let short = read_record(Kind::Update, {
            let mut update = header("main", "t").expect("a name");
            put_runs(&mut update, &[1]).expect("one row");
            put_rows(&mut update, &fields, &[chunk], usize::MAX).expect("rows");
            update.into()
        })
        .expect("reads");
        assert!(short.change(&fields).is_err(), "five rows for one row number");
    }
}
