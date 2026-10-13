//! What a sort hands its rows to, whether or not they fitted.
//!
//! A sort that fits keeps its answer as chunks and this is [`crate::buffer::Buffered`] with a
//! different name. A sort that does not fit writes sorted runs to files as it goes, and then this
//! is the merge over them, which is the case the module exists for.
//!
//! # Why the merge is here and not in the sort
//!
//! Section 15.6 of `tenx/15-the-partitioned-write.md` is about a measurement. A sink's peak is
//! inside its own `finalize`, where it is holding its input and the answer it is assembling from
//! it at once, and nothing a downstream reader does can lower a peak the query is already past.
//! The drain that was tried first, where the buffer hands a chunk over instead of copying it, moved
//! the smallest limit a clustered SF1 load runs under by nothing at all, and a build that handed
//! the whole charge back the instant the buffer was filled moved it by nothing either.
//!
//! So an operator that wants to bound its memory cannot do it by being careful about what it hands
//! over. It has to never have the answer in hand, which means the merge runs as the rows are asked
//! for rather than before. That is what this is: a [`Source`] that holds one chunk per run and one
//! chunk of output, and reads the next piece of each run only when the one before it has gone.
//!
//! # What a run carries beside its rows
//!
//! One extra column, a fixed forty bytes: the normalized sort key of the row and then where the row
//! arrived, most significant byte first. Comparing two of those as bytes is comparing the rows the
//! way the sort compares them, key first and arrival settling a tie, so the merge needs no key
//! expressions, no evaluation and no types beyond the payload's own.
//!
//! The arrival is in there for the reason it is in the sort at all. It is unique per row and it is
//! the last thing compared, so no two rows are ever equal and the answer does not depend on how
//! many threads ran or, now, on where the spills happened to fall.

use std::collections::BTreeMap;
use std::fmt;
use std::ops::Range;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use rudb_common::{Error, LogicalType, Result, Value};
use rudb_pipeline::{Morsel, Progress, Source};
use rudb_vector::{Assembly, Chunk, Data, StringColumn, VECTOR_SIZE, Vector};

use crate::runs::{Reader, Runs};

/// How many bytes of a run's ordering column are the normalized key.
pub(crate) const KEY: usize = crate::normal::WIDTH;

/// How wide a run's ordering column is: the key and then the arrival.
pub(crate) const ORDER: usize = KEY + 16;

/// How many rows a piece of a merge holds before the next key starts a new one.
///
/// A piece is a morsel, and the load under a clustered sort never lets a stripe span two of them,
/// so a piece is at least two full stripes wherever the keys allow it. Past that a bigger piece
/// only means fewer of them to spread over the threads.
const PIECE_ROWS: usize = 1 << 20;

/// One sorted run, and where in it each value of the leading key starts.
#[derive(Debug)]
pub(crate) struct Run {
    pub(crate) file: Runs,
    /// The run's stretches in key order, or `None` for a run whose leading key changes too often
    /// for its stretches to be worth listing.
    pub(crate) stretches: Option<Vec<Stretch>>,
}

/// The rows of one run that share a value of the leading sort key.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Stretch {
    /// The normalized bytes of that value, which order the way the values do.
    pub(crate) key: Vec<u8>,
    /// The run's blocks those rows are in, which hold no other rows.
    pub(crate) blocks: Range<usize>,
    pub(crate) rows: usize,
}

/// The rows of a finished sort, however they ended up being held.
///
/// Cloning one gives another handle on the same rows and the same cursor, which is how the sink
/// half and the source half of a sort end up looking at the same thing.
#[derive(Debug, Clone)]
pub(crate) struct Sorted {
    stage: Arc<Mutex<Stage>>,
    handed: Arc<AtomicU64>,
    /// How many rows the sink reading this wants in one morsel, or zero for no wish. See
    /// [`Source::gather`].
    asked: Arc<AtomicUsize>,
}

/// Where a sort's answer is.
#[derive(Debug)]
enum Stage {
    /// Nothing has been handed over yet, which is every moment before the sort finalises.
    Waiting,
    /// It all fitted, so this is the answer.
    Held(Vec<Chunk>),
    /// It did not, so this is the runs it went to, cut into pieces that merge on their own.
    Merging(Arc<Pieces>),
}

/// A merge cut where the leading key changes, so that each piece is a merge of its own over one
/// stretch of every run and the pieces can run at once.
///
/// A run is sorted, so its rows for one value of the leading key are one stretch of it, and a
/// spilled run is cut into blocks at those places. A piece covering a range of values is then a
/// merge over a range of blocks of each run, and the pieces in key order are the whole answer in
/// order. That is what a clustered load needs, since its leading key is the partition and has a
/// few hundred values at most: one merge of every run was one thread encoding the whole table.
#[derive(Debug)]
struct Pieces {
    files: Vec<Arc<Runs>>,
    pieces: Vec<Mutex<Piece>>,
    types: Vec<LogicalType>,
}

/// How far one piece of a merge has got.
#[derive(Debug)]
enum Piece {
    /// Nothing read yet: the blocks of each run the piece covers, by the run's place in
    /// [`Pieces::files`].
    Waiting(Vec<(usize, Range<usize>)>),
    Merging(Merge),
    Spent,
}

impl Default for Sorted {
    fn default() -> Self {
        Self::new()
    }
}

impl Sorted {
    /// A handle nothing has been handed to yet.
    pub(crate) fn new() -> Self {
        Self {
            stage: Arc::new(Mutex::new(Stage::Waiting)),
            handed: Arc::new(AtomicU64::new(0)),
            asked: Arc::new(AtomicUsize::new(0)),
        }
    }

    /// Hands over an answer that fitted.
    ///
    /// # Errors
    ///
    /// [`ErrorCode::Internal`](rudb_common::ErrorCode::Internal) if a thread panicked while holding
    /// the stage.
    pub(crate) fn hold(&self, chunks: Vec<Chunk>) -> Result<()> {
        *self.stage.lock().map_err(poisoned)? = Stage::Held(chunks);
        Ok(())
    }

    /// Hands over the runs an answer that did not fit went to.
    ///
    /// # Errors
    ///
    /// The same as [`Sorted::hold`], and whatever reading the first chunk of each run reports.
    pub(crate) fn merge(&self, files: Vec<Run>, types: Vec<LogicalType>) -> Result<()> {
        self.merge_in(files, types, PIECE_ROWS)
    }

    /// [`Sorted::merge`] with pieces of `rows` rows.
    fn merge_in(&self, files: Vec<Run>, types: Vec<LogicalType>, rows: usize) -> Result<()> {
        let mut finished = Vec::with_capacity(files.len());
        let mut stretches = Vec::with_capacity(files.len());
        for Run { mut file, stretches: listed } in files {
            file.finish()?;
            finished.push(Arc::new(file));
            stretches.push(listed);
        }
        let pieces = cut(&finished, &stretches, rows)
            .into_iter()
            .map(|blocks| Mutex::new(Piece::Waiting(blocks)))
            .collect();
        *self.stage.lock().map_err(poisoned)? =
            Stage::Merging(Arc::new(Pieces { files: finished, pieces, types }));
        Ok(())
    }
}

/// The pieces a merge of `files` is cut into, each as the blocks of every run it covers, in key
/// order.
///
/// One piece of everything unless every run listed its stretches. Otherwise the values of the
/// leading key are taken in order and a piece closes at the first value that takes it to `rows`
/// rows, so a value is never split between two pieces and a piece holds whole stripes.
fn cut(
    files: &[Arc<Runs>],
    stretches: &[Option<Vec<Stretch>>],
    rows: usize,
) -> Vec<Vec<(usize, Range<usize>)>> {
    let whole = || {
        files
            .iter()
            .enumerate()
            .map(|(at, file)| (at, 0..usize::try_from(file.chunks()).unwrap_or(usize::MAX)))
            .collect::<Vec<_>>()
    };
    let Some(listed) = stretches.iter().map(Option::as_deref).collect::<Option<Vec<_>>>() else {
        return vec![whole()];
    };
    let mut counts: BTreeMap<&[u8], usize> = BTreeMap::new();
    for stretch in listed.iter().flat_map(|stretches| stretches.iter()) {
        *counts.entry(stretch.key.as_slice()).or_default() += stretch.rows;
    }
    // Where each piece after the first starts, as the first value it holds.
    let mut starts: Vec<&[u8]> = Vec::new();
    let mut held = 0;
    for (key, count) in counts {
        if held >= rows {
            starts.push(key);
            held = 0;
        }
        held += count;
    }
    if starts.is_empty() {
        return vec![whole()];
    }
    let mut pieces = vec![Vec::new(); starts.len() + 1];
    for (at, stretches) in listed.iter().enumerate() {
        let mut from = 0;
        for (piece, blocks) in pieces.iter_mut().enumerate() {
            let to = starts.get(piece).map_or(stretches.len(), |start| {
                from + stretches[from..].partition_point(|stretch| stretch.key.as_slice() < *start)
            });
            if from < to {
                blocks.push((at, stretches[from].blocks.start..stretches[to - 1].blocks.end));
            }
            from = to;
        }
    }
    pieces
}

/// How many of a fitted sort's `chunks` go in one morsel, for a sink that asked for `asked` rows in
/// one.
///
/// A load into a native file is the sink that asks. It never lets a stripe span two morsels, so a
/// morsel a chunk was a stripe a chunk: a clustered `lineitem` of three million rows went to the
/// file as 367 stripes of one part each. The first stripe is also the one a varchar column's
/// dictionary is decided on, and eight thousand rows of a table sorted on a date held a new
/// `l_comment` in nearly every row, so the column lost its dictionary and every part of it was an
/// FSST page with a symbol table of its own.
fn per_morsel(chunks: &[Chunk], asked: usize) -> usize {
    let rows = chunks.iter().map(Chunk::len).sum::<usize>();
    let each = rows.div_ceil(chunks.len().max(1)).max(1);
    (asked / each).max(1)
}

impl Source for Sorted {
    fn morsel(&self) -> Option<Morsel> {
        let index = self.handed.fetch_add(1, Ordering::Relaxed);
        match &*self.stage.lock().ok()? {
            // One morsel a chunk, the same as any finished operator's buffer, unless the sink asked
            // for more rows than that.
            Stage::Held(chunks) => {
                let per = per_morsel(chunks, self.asked.load(Ordering::Relaxed)) as u64;
                let start = index.checked_mul(per)?;
                let end = start.saturating_add(per).min(chunks.len() as u64);
                (start < end).then(|| Morsel::new(index, start, end))
            }
            // A morsel a piece. A piece is a single pass over its stretch of every run and a second
            // thread taking a morsel of it would be a second pass, which is a different answer and
            // not a faster one.
            Stage::Merging(pieces) => {
                (index < pieces.pieces.len() as u64).then(|| Morsel::new(index, 0, 1))
            }
            Stage::Waiting => (index == 0).then(|| Morsel::new(0, 0, 1)),
        }
    }

    fn morsels(&self, threads: usize, _weight: usize) -> Option<usize> {
        let stage = self.stage.lock().ok()?;
        match &*stage {
            Stage::Held(chunks) => {
                let rows = chunks.iter().map(Chunk::len).sum::<usize>();
                let asked = self.asked.load(Ordering::Relaxed);
                // Four instances at most for whatever reads a sorted answer, unless the sink asked
                // for runs of chunks. That is a load into a native file, which encodes every row it
                // takes and is the slowest thing in its pipeline, and at four instances the encode
                // of a clustered load ran on four of six cores on `server2` after the sort was done.
                let most = if asked > 0 { usize::MAX } else { 4 };
                let useful = rows.div_ceil(50_000).clamp(1, most);
                let morsels = chunks.len().div_ceil(per_morsel(chunks, asked));
                Some(morsels.min(threads).min(useful))
            }
            Stage::Waiting => Some(0),
            Stage::Merging(pieces) => Some(pieces.pieces.len()),
        }
    }

    /// Remembered for [`Sorted::morsel`], which hands a fitted sort out a run of chunks at a time
    /// up to this many rows.
    fn gather(&self, rows: usize) {
        self.asked.store(rows, Ordering::Relaxed);
    }

    fn read(&self, morsel: &mut Morsel, out: &mut Chunk) -> Result<Progress> {
        let stage = self.stage.lock().map_err(poisoned)?;
        match &*stage {
            Stage::Held(chunks) => {
                let index = morsel.cursor() as usize;
                let Some(chunk) = chunks.get(index) else {
                    return Err(Error::internal(format!("{morsel} asks for a chunk nobody built")));
                };
                *out = chunk.clone();
                morsel.advance(1);
                Ok(if morsel.is_drained() { Progress::Done } else { Progress::More })
            }
            Stage::Waiting => Err(Error::internal("a sort was read before it finished")),
            Stage::Merging(pieces) => {
                // Let go of the stage before the merge, which is what lets the pieces run at once.
                let pieces = Arc::clone(pieces);
                drop(stage);
                pieces.read(morsel, out)
            }
        }
    }
}

impl Pieces {
    /// The next chunk of the piece `morsel` is, opening the piece on its first read.
    fn read(&self, morsel: &mut Morsel, out: &mut Chunk) -> Result<Progress> {
        let Some(piece) = usize::try_from(morsel.index()).ok().and_then(|at| self.pieces.get(at))
        else {
            return Err(Error::internal(format!("{morsel} asks for a piece nobody cut")));
        };
        let mut piece = piece.lock().map_err(poisoned)?;
        if let Piece::Waiting(blocks) = &*piece {
            let mut heads = Vec::with_capacity(blocks.len());
            for (file, blocks) in blocks {
                let Some(file) = self.files.get(*file) else {
                    return Err(Error::internal("a piece of a merge names a run it does not have"));
                };
                heads.push(Head::opening(file.reader(blocks.clone())?)?);
            }
            *piece = Piece::Merging(Merge { heads, types: self.types.clone() });
        }
        let next = match &mut *piece {
            Piece::Merging(merge) => merge.next()?.map(|chunk| (chunk, merge.spent())),
            Piece::Waiting(_) | Piece::Spent => None,
        };
        match next {
            // Done only when the runs are empty, because a morsel that drains on its first chunk
            // would take the rest of the piece with it.
            Some((chunk, false)) => {
                *out = chunk;
                Ok(Progress::More)
            }
            Some((chunk, true)) => {
                *out = chunk;
                *piece = Piece::Spent;
                morsel.advance(1);
                Ok(Progress::Done)
            }
            None => {
                *out = Chunk::empty(&self.types);
                *piece = Piece::Spent;
                morsel.advance(1);
                Ok(Progress::Done)
            }
        }
    }
}

/// A k way merge over sorted runs, one chunk of each resident at a time.
#[derive(Debug)]
struct Merge {
    heads: Vec<Head>,
    types: Vec<LogicalType>,
}

impl Merge {
    /// Whether every run has given up everything it had.
    fn spent(&self) -> bool {
        self.heads.iter().all(|head| head.at >= head.chunk.len())
    }

    /// The run whose next row comes first, or `None` when they are all spent.
    fn best(&self) -> Option<usize> {
        let mut best: Option<(usize, &[u8])> = None;
        for (index, head) in self.heads.iter().enumerate() {
            let Some(order) = head.order() else { continue };
            match best {
                Some((_, so_far)) if order >= so_far => {}
                _ => best = Some((index, order)),
            }
        }
        best.map(|(index, _)| index)
    }

    /// The next chunk of the answer, or `None` once the runs are spent.
    ///
    /// Rows are taken from one run for as long as that run keeps winning, so what comes out of the
    /// loop is a list of ranges rather than a list of rows. That matters for more than tidiness: a
    /// range is a slice of a chunk the run already read, which shares its pages, so the copying is
    /// one pass at the end over whole columns rather than a value at a time.
    fn next(&mut self) -> Result<Option<Chunk>> {
        let mut pieces: Vec<Chunk> = Vec::new();
        let mut rows = 0usize;
        while rows < VECTOR_SIZE {
            let Some(winner) = self.best() else { break };
            let from = self.heads[winner].at;
            loop {
                self.heads[winner].at += 1;
                rows += 1;
                if rows == VECTOR_SIZE || self.heads[winner].at >= self.heads[winner].chunk.len() {
                    break;
                }
                if self.best() != Some(winner) {
                    break;
                }
            }
            let head = &self.heads[winner];
            pieces.push(head.slice(from, head.at - from)?);
            if self.heads[winner].at >= self.heads[winner].chunk.len() {
                self.heads[winner].refill()?;
            }
        }
        if rows == 0 {
            return Ok(None);
        }
        Ok(Some(laid(&self.types, &pieces, rows)?))
    }
}

/// The pieces written out as one chunk, each column laid once.
///
/// The same [`Assembly`] the sort itself finishes with, and for the same reason: an interleave of
/// several pieces into one column is a typed copy per physical layout rather than a value a field.
/// The positions are consecutive here, because the pieces are already in the order they go in.
fn laid(types: &[LogicalType], pieces: &[Chunk], rows: usize) -> Result<Chunk> {
    let mut columns = Vec::with_capacity(types.len());
    for (position, ty) in types.iter().enumerate() {
        let mut assembly = Assembly::new(ty.clone(), rows)?;
        let mut at = 0u32;
        for piece in pieces {
            let column = piece.column(position)?;
            let places: Vec<u32> = (0..column.len() as u32).map(|row| at + row).collect();
            assembly.place(&places, column)?;
            at += column.len() as u32;
        }
        columns.push(assembly.finish()?);
    }
    Chunk::with_rows(columns, rows)
}

/// One sorted run, with the chunk of it the merge is currently looking at.
#[derive(Debug)]
struct Head {
    file: Reader,
    /// The payload of the chunk being read, with the ordering column taken off.
    chunk: Chunk,
    /// The ordering column of that chunk, which is what the merge compares.
    order: Vector,
    at: usize,
}

impl Head {
    /// A run with its first chunk read.
    fn opening(file: Reader) -> Result<Self> {
        let empty = Vector::constant(LogicalType::Blob, Value::Null, 0);
        let mut head = Self { file, chunk: Chunk::empty(&[]), order: empty, at: 0 };
        head.refill()?;
        Ok(head)
    }

    /// The bytes the merge compares this run by, or `None` when it has nothing left.
    fn order(&self) -> Option<&[u8]> {
        if self.at >= self.chunk.len() {
            return None;
        }
        self.order.bytes_at(self.at)
    }

    /// `len` rows of the payload starting at `from`, sharing the pages rather than copying them.
    fn slice(&self, from: usize, len: usize) -> Result<Chunk> {
        let mut columns = Vec::with_capacity(self.chunk.width());
        for column in self.chunk.columns() {
            columns.push(column.slice(from, len)?);
        }
        Chunk::with_rows(columns, len)
    }

    /// Reads the next chunk of this run, leaving it spent when there is none.
    fn refill(&mut self) -> Result<()> {
        loop {
            let Some(chunk) = self.file.next_chunk()? else {
                self.chunk = Chunk::empty(&[]);
                self.at = 0;
                return Ok(());
            };
            if chunk.is_empty() {
                continue;
            }
            let rows = chunk.len();
            let mut columns = chunk.into_columns();
            let Some(order) = columns.pop() else {
                return Err(Error::internal("a sorted run with no ordering column in it"));
            };
            self.order = order;
            self.chunk = Chunk::with_rows(columns, rows)?;
            self.at = 0;
            return Ok(());
        }
    }
}

/// The bytes a row is ordered by: its normalized key, then where it arrived.
///
/// Most significant byte first for the arrival, because the whole point is that comparing two of
/// these as bytes is comparing the rows, and a little endian number does not compare as bytes.
pub(crate) fn order_of(key: &[u8; KEY], arrival: (u64, u64)) -> [u8; ORDER] {
    let mut out = [0u8; ORDER];
    out[..KEY].copy_from_slice(key);
    out[KEY..KEY + 8].copy_from_slice(&arrival.0.to_be_bytes());
    out[KEY + 8..].copy_from_slice(&arrival.1.to_be_bytes());
    out
}

/// Those bytes as the extra column a run carries.
///
/// A `BLOB` because that is the type whose comparison is the byte comparison, which means nothing
/// downstream of the run file has to know what the bytes mean. Forty of them is past the sixteen a
/// string view holds inline, so they go to the arena, and the column is one allocation for the
/// block rather than one a row.
///
/// # Errors
///
/// If the column cannot be built, which for a `BLOB` of known bytes it cannot.
pub(crate) fn ordering(orders: &[[u8; ORDER]]) -> Result<Vector> {
    let mut strings = StringColumn::with_capacity(orders.len());
    for order in orders {
        strings.push_bytes(order);
    }
    Vector::flat(LogicalType::Blob, Data::Varlen(strings))
}

fn poisoned<T>(_: T) -> Error {
    Error::internal("a thread panicked while holding a sort's finished rows")
}

impl fmt::Display for Sorted {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self.stage.lock() {
            Ok(stage) => match &*stage {
                Stage::Held(chunks) => write!(f, "{} sorted chunks", chunks.len()),
                Stage::Waiting => write!(f, "a sort that has not finished"),
                Stage::Merging(pieces) => {
                    let rows: u64 = pieces.files.iter().map(|file| file.rows()).sum();
                    let bytes: u64 = pieces.files.iter().map(|file| file.bytes()).sum();
                    let chunks: u64 = pieces.files.iter().map(|file| file.chunks()).sum();
                    write!(
                        f,
                        "a merge of {} runs in {} pieces, {rows} rows in {chunks} chunks and \
                         {bytes} bytes",
                        pieces.files.len(),
                        pieces.pieces.len()
                    )
                }
            },
            Err(_) => write!(f, "sorted rows nobody can read"),
        }
    }
}

#[cfg(test)]
mod tests {
    use rudb_common::{LogicalType, Value};
    use rudb_vector::{Data, Vector};

    use super::{
        Chunk, KEY, ORDER, Progress, Run, Runs, Sorted, Source, Stretch, VECTOR_SIZE, order_of,
        ordering,
    };

    /// A run of the given keys, each one a row whose payload is the key as a `BIGINT`.
    ///
    /// The arrival is the run's number and the row's place in it, which is what a real spill would
    /// have written and is what settles a tie between two runs.
    fn run(number: u64, keys: &[i64]) -> Runs {
        cut_at(number, keys, &[]).0
    }

    /// [`run`] cut into blocks at `starts` as well, with the blocks of each stretch.
    fn cut_at(number: u64, keys: &[i64], starts: &[usize]) -> (Runs, Vec<std::ops::Range<usize>>) {
        let types = vec![LogicalType::BigInt, LogicalType::Blob];
        let mut file = Runs::new("test", types).expect("a run file");
        let stretches = file.begin_at(keys.len(), starts).expect("the row count");
        let payload = Vector::flat(LogicalType::BigInt, Data::Int64(keys.to_vec().into()))
            .expect("bigints are an i64 layout");
        file.column(&payload).expect("the payload column");
        let orders: Vec<[u8; ORDER]> = keys
            .iter()
            .enumerate()
            .map(|(at, key)| {
                let mut bytes = [0u8; KEY];
                // Flipped top bit, which is what `normal` writes for a signed key so that the byte
                // order is the numeric order.
                bytes[..8].copy_from_slice(&(*key as u64 ^ (1 << 63)).to_be_bytes());
                order_of(&bytes, (number, at as u64))
            })
            .collect();
        let blocks: Vec<_> = file.blocks().collect();
        for block in blocks {
            file.part(&ordering(&orders[block]).expect("blobs")).expect("the ordering column");
        }
        (file, stretches)
    }

    /// A run cut wherever its key changes, with its stretches listed, which is what a sort that
    /// spilled writes when the key is the leading one.
    fn listed(number: u64, keys: &[i64]) -> Run {
        let starts: Vec<usize> =
            (0..keys.len()).filter(|&row| row == 0 || keys[row] != keys[row - 1]).collect();
        let (file, blocks) = cut_at(number, keys, &starts);
        let stretches = starts
            .iter()
            .zip(blocks.into_iter().skip(1))
            .enumerate()
            .map(|(at, (&start, blocks))| Stretch {
                key: (keys[start] as u64 ^ (1 << 63)).to_be_bytes().to_vec(),
                blocks,
                rows: starts.get(at + 1).copied().unwrap_or(keys.len()) - start,
            })
            .collect();
        Run { file, stretches: Some(stretches) }
    }

    /// Everything that comes out of `sorted`, a morsel at a time, with the morsels in the order
    /// they were handed out.
    fn drained(sorted: &Sorted) -> Vec<Vec<i64>> {
        let mut out = Vec::new();
        while let Some(mut morsel) = sorted.morsel() {
            let mut values = Vec::new();
            loop {
                let mut chunk = Chunk::empty(&[]);
                let progress = sorted.read(&mut morsel, &mut chunk).expect("a chunk");
                for row in 0..chunk.len() {
                    match chunk.value_at(row, 0) {
                        Value::BigInt(value) => values.push(value),
                        other => panic!("a {other} came out of a BIGINT column"),
                    }
                }
                if progress == Progress::Done {
                    break;
                }
            }
            out.push(values);
        }
        out
    }

    /// Everything a merge of these runs gives back, in the order it gives it.
    fn merged(files: Vec<Runs>) -> Vec<i64> {
        let sorted = Sorted::new();
        let files = files.into_iter().map(|file| Run { file, stretches: None }).collect();
        sorted.merge(files, vec![LogicalType::BigInt]).expect("the runs open");
        drained(&sorted).concat()
    }

    /// Runs that list their stretches merge as pieces, each piece a run of whole keys, and the
    /// pieces in the order they are handed out are the whole answer in order.
    #[test]
    fn runs_cut_by_key_merge_as_pieces_in_key_order() {
        let first: Vec<i64> = (0..VECTOR_SIZE as i64 + 30).map(|row| row / 100).collect();
        let second: Vec<i64> = (0..500).map(|row| row / 50 + 3).collect();
        let mut want: Vec<i64> = first.iter().chain(second.iter()).copied().collect();
        want.sort_unstable();
        let sorted = Sorted::new();
        let files = vec![listed(0, &first), listed(1, &second)];
        sorted.merge_in(files, vec![LogicalType::BigInt], 300).expect("the runs open");
        let pieces = sorted.morsels(16, 1).expect("merging");
        assert!(pieces > 3, "{pieces} pieces");
        let answer = drained(&sorted);
        assert_eq!(answer.len(), pieces);
        for pair in answer.windows(2) {
            let (Some(last), Some(next)) = (pair[0].last(), pair[1].first()) else {
                panic!("an empty piece");
            };
            assert!(last < next, "a key split between two pieces");
        }
        assert_eq!(answer.concat(), want);
    }

    /// One run that did not list its stretches is enough to merge everything as one piece, since
    /// nothing says where its keys are.
    #[test]
    fn a_run_without_stretches_merges_as_one_piece() {
        let sorted = Sorted::new();
        let files =
            vec![listed(0, &[1, 1, 2, 3]), Run { file: run(1, &[1, 2, 2, 4]), stretches: None }];
        sorted.merge_in(files, vec![LogicalType::BigInt], 1).expect("the runs open");
        assert_eq!(sorted.morsels(16, 1), Some(1));
        assert_eq!(drained(&sorted).concat(), vec![1, 1, 1, 2, 2, 2, 3, 4]);
    }

    #[test]
    fn two_runs_come_out_in_one_order() {
        let answer = merged(vec![run(0, &[1, 4, 7, 9]), run(1, &[2, 3, 8])]);
        assert_eq!(answer, vec![1, 2, 3, 4, 7, 8, 9]);
    }

    /// The run a tie goes to is the one that arrived first, which is what the arrival in the
    /// ordering bytes is for. Without it the answer here would depend on which run the loop looked
    /// at first, which is the thing a stable sort promises not to do.
    #[test]
    fn a_tie_goes_to_whichever_row_arrived_first() {
        // Both rows are 5, so the payload cannot say which came first and the ordering bytes have
        // to. The run written second is handed to the merge first, so a merge that took the runs in
        // the order it was given them would get this backwards.
        assert_eq!(merged(vec![run(1, &[5, 5]), run(0, &[5, 5])]).len(), 4);
        assert!(order_of(&[0; KEY], (0, 0)) < order_of(&[0; KEY], (1, 0)));
        assert!(order_of(&[0; KEY], (0, 0)) < order_of(&[0; KEY], (0, 1)));
    }

    /// Runs longer than a block, so the merge has to read the next chunk of each of them partway
    /// through and the two runs run out of blocks at different points.
    #[test]
    fn a_run_of_several_chunks_is_read_through() {
        let rows = VECTOR_SIZE * 2 + 5;
        let long: Vec<i64> = (0..rows as i64).map(|value| value * 2).collect();
        let short: Vec<i64> = (0..rows as i64 - VECTOR_SIZE as i64).map(|v| v * 2 + 1).collect();
        let mut want: Vec<i64> = long.iter().chain(short.iter()).copied().collect();
        want.sort_unstable();
        assert_eq!(merged(vec![run(0, &long), run(1, &short)]), want);
    }

    #[test]
    fn one_run_comes_back_as_itself() {
        assert_eq!(merged(vec![run(0, &[3, 4, 5])]), vec![3, 4, 5]);
    }

    #[test]
    fn runs_with_nothing_in_them_merge_to_nothing() {
        assert!(merged(vec![run(0, &[]), run(1, &[])]).is_empty());
    }

    /// Negative keys sort below positive ones, which is the flipped sign bit doing its job and is
    /// worth a test here because the merge never looks at the payload.
    #[test]
    fn the_key_bytes_order_the_way_the_numbers_do() {
        let answer = merged(vec![run(0, &[-9, -1, 3]), run(1, &[-5, 0, 7])]);
        assert_eq!(answer, vec![-9, -5, -1, 0, 3, 7]);
    }

    #[test]
    fn a_sort_that_fitted_reads_back_the_chunks_it_was_given() {
        let column = Vector::flat(LogicalType::BigInt, Data::Int64(vec![1, 2].into()))
            .expect("bigints are an i64 layout");
        let sorted = Sorted::new();
        sorted.hold(vec![Chunk::new(vec![column]).expect("one column")]).expect("held");
        let mut morsel = sorted.morsel().expect("the one chunk");
        let mut out = Chunk::empty(&[]);
        assert_eq!(sorted.read(&mut morsel, &mut out).expect("readable"), Progress::Done);
        assert_eq!(out.value_at(0, 0), Value::BigInt(1));
        assert!(sorted.morsel().is_none(), "and no second chunk");
    }

    /// A sink that asks for more rows than a chunk gets runs of chunks, in order, and the last run
    /// is whatever is left.
    #[test]
    fn a_sort_that_fitted_hands_out_runs_of_chunks_when_asked() {
        let chunks = (0..5)
            .map(|at| {
                let column = Vector::flat(LogicalType::BigInt, Data::Int64(vec![at, at].into()))
                    .expect("bigints are an i64 layout");
                Chunk::new(vec![column]).expect("one column")
            })
            .collect();
        let sorted = Sorted::new();
        sorted.hold(chunks).expect("held");
        sorted.gather(4);
        assert_eq!(sorted.morsels(16, 1), Some(1), "ten rows are one instance's worth");
        let mut seen = Vec::new();
        while let Some(mut morsel) = sorted.morsel() {
            let mut first = Vec::new();
            loop {
                let mut out = Chunk::empty(&[]);
                let progress = sorted.read(&mut morsel, &mut out).expect("readable");
                first.push(out.value_at(0, 0));
                if progress == Progress::Done {
                    break;
                }
            }
            seen.push(first);
        }
        let bigint = Value::BigInt;
        assert_eq!(
            seen,
            vec![vec![bigint(0), bigint(1)], vec![bigint(2), bigint(3)], vec![bigint(4)]]
        );
    }

    /// A large answer is read by four instances at most, and by as many as there are threads when
    /// the sink asked for runs of chunks.
    #[test]
    fn a_sink_that_asks_for_runs_of_a_large_sort_gets_every_thread() {
        let held = || {
            let sorted = Sorted::new();
            let chunks = (0..200)
                .map(|at| {
                    let column =
                        Vector::flat(LogicalType::BigInt, Data::Int64(vec![at; 2048].into()))
                            .expect("bigints are an i64 layout");
                    Chunk::new(vec![column]).expect("one column")
                })
                .collect();
            sorted.hold(chunks).expect("held");
            sorted
        };
        assert_eq!(held().morsels(16, 1), Some(4), "a query reads it on four");
        let asking = held();
        asking.gather(8192);
        assert_eq!(asking.morsels(16, 1), Some(9), "409,600 rows are nine of 50,000");
        let asking = held();
        asking.gather(8192);
        assert_eq!(asking.morsels(6, 1), Some(6), "and no more than there are threads");
        let small = held();
        small.gather(1 << 20);
        assert_eq!(small.morsels(16, 1), Some(1), "and no more than there are morsels");
    }

    #[test]
    fn a_sort_that_has_not_finished_says_so_rather_than_answering_nothing() {
        let sorted = Sorted::new();
        let mut morsel = rudb_pipeline::Morsel::new(0, 0, 1);
        let mut out = Chunk::empty(&[]);
        let why = sorted.read(&mut morsel, &mut out).expect_err("nothing was handed over");
        assert!(why.to_string().contains("read before it finished"), "{why}");
    }
}
