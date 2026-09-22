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

use std::fmt;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use rudb_common::{Error, LogicalType, Result, Value};
use rudb_pipeline::{Morsel, Progress, Source};
use rudb_vector::{Assembly, Chunk, Data, StringColumn, VECTOR_SIZE, Vector};

use crate::runs::Runs;

/// How many bytes of a run's ordering column are the normalized key.
pub(crate) const KEY: usize = crate::normal::WIDTH;

/// How wide a run's ordering column is: the key and then the arrival.
pub(crate) const ORDER: usize = KEY + 16;

/// The rows of a finished sort, however they ended up being held.
///
/// Cloning one gives another handle on the same rows and the same cursor, which is how the sink
/// half and the source half of a sort end up looking at the same thing.
#[derive(Debug, Clone)]
pub(crate) struct Sorted {
    stage: Arc<Mutex<Stage>>,
    handed: Arc<AtomicU64>,
}

/// Where a sort's answer is.
#[derive(Debug)]
enum Stage {
    /// Nothing has been handed over yet, which is every moment before the sort finalises.
    Waiting,
    /// It all fitted, so this is the answer.
    Held(Vec<Chunk>),
    /// It did not, so this is the runs it went to and how far the merge has got.
    Merging(Merge),
}

impl Default for Sorted {
    fn default() -> Self {
        Self::new()
    }
}

impl Sorted {
    /// A handle nothing has been handed to yet.
    pub(crate) fn new() -> Self {
        Self { stage: Arc::new(Mutex::new(Stage::Waiting)), handed: Arc::new(AtomicU64::new(0)) }
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
    pub(crate) fn merge(&self, files: Vec<Runs>, types: Vec<LogicalType>) -> Result<()> {
        let mut heads = Vec::with_capacity(files.len());
        for file in files {
            heads.push(Head::opening(file)?);
        }
        *self.stage.lock().map_err(poisoned)? = Stage::Merging(Merge { heads, types });
        Ok(())
    }

    /// How many chunks are waiting, which only a sort that fitted can answer.
    fn held(&self) -> Option<usize> {
        match &*self.stage.lock().ok()? {
            Stage::Held(chunks) => Some(chunks.len()),
            Stage::Waiting | Stage::Merging(_) => None,
        }
    }
}

impl Source for Sorted {
    fn morsel(&self) -> Option<Morsel> {
        let index = self.handed.fetch_add(1, Ordering::Relaxed);
        match self.held() {
            // One morsel a chunk, the same as any finished operator's buffer.
            Some(chunks) if index < chunks as u64 => Some(Morsel::new(index, index, index + 1)),
            Some(_) => None,
            // One morsel ever. A merge is a single pass over every run at once and a second thread
            // taking a morsel of it would be a second pass, which is a different answer and not a
            // faster one.
            None if index == 0 => Some(Morsel::new(0, 0, 1)),
            None => None,
        }
    }

    fn morsels(&self, threads: usize, _weight: usize) -> Option<usize> {
        let stage = self.stage.lock().ok()?;
        match &*stage {
            Stage::Held(chunks) => {
                let rows = chunks.iter().map(Chunk::len).sum::<usize>();
                let useful = rows.div_ceil(50_000).clamp(1, 4);
                Some(chunks.len().min(threads).min(useful))
            }
            Stage::Waiting => Some(0),
            Stage::Merging(_) => Some(1),
        }
    }

    fn read(&self, morsel: &mut Morsel, out: &mut Chunk) -> Result<Progress> {
        let mut stage = self.stage.lock().map_err(poisoned)?;
        match &mut *stage {
            Stage::Held(chunks) => {
                let index = morsel.cursor() as usize;
                let Some(chunk) = chunks.get(index) else {
                    return Err(Error::internal(format!("{morsel} asks for a chunk nobody built")));
                };
                *out = chunk.clone();
                morsel.advance(1);
                Ok(Progress::Done)
            }
            Stage::Waiting => Err(Error::internal("a sort was read before it finished")),
            Stage::Merging(merge) => match merge.next()? {
                Some(chunk) => {
                    *out = chunk;
                    // Done only when the runs are empty, because a morsel that drains on its first
                    // chunk would take the rest of the merge with it.
                    if merge.spent() {
                        morsel.advance(1);
                        return Ok(Progress::Done);
                    }
                    Ok(Progress::More)
                }
                None => {
                    *out = Chunk::empty(&merge.types);
                    morsel.advance(1);
                    Ok(Progress::Done)
                }
            },
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
    file: Runs,
    /// The payload of the chunk being read, with the ordering column taken off.
    chunk: Chunk,
    /// The ordering column of that chunk, which is what the merge compares.
    order: Vector,
    at: usize,
}

impl Head {
    /// A run with its first chunk read.
    fn opening(file: Runs) -> Result<Self> {
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
                Stage::Merging(merge) => {
                    let rows: u64 = merge.heads.iter().map(|head| head.file.rows()).sum();
                    let bytes: u64 = merge.heads.iter().map(|head| head.file.bytes()).sum();
                    let chunks: u64 = merge.heads.iter().map(|head| head.file.chunks()).sum();
                    write!(
                        f,
                        "a merge of {} runs, {rows} rows in {chunks} chunks and {bytes} bytes",
                        merge.heads.len()
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

    use super::{Chunk, KEY, ORDER, Progress, Runs, Sorted, Source, order_of, ordering};

    /// A run of the given keys, each one a row whose payload is the key as a `BIGINT`.
    ///
    /// The arrival is the run's number and the row's place in it, which is what a real spill would
    /// have written and is what settles a tie between two runs.
    fn run(number: u64, keys: &[i64], per: usize) -> Runs {
        let types = vec![LogicalType::BigInt, LogicalType::Blob];
        let mut file = Runs::new("test", types).expect("a run file");
        for (block, rows) in keys.chunks(per).enumerate() {
            let payload = Vector::flat(LogicalType::BigInt, Data::Int64(rows.to_vec().into()))
                .expect("bigints are an i64 layout");
            let orders: Vec<[u8; ORDER]> = rows
                .iter()
                .enumerate()
                .map(|(at, key)| {
                    let mut bytes = [0u8; KEY];
                    // Flipped top bit, which is what `normal` writes for a signed key so that the
                    // byte order is the numeric order.
                    bytes[..8].copy_from_slice(&(*key as u64 ^ (1 << 63)).to_be_bytes());
                    order_of(&bytes, (number, (block * per + at) as u64))
                })
                .collect();
            let chunk = Chunk::new(vec![payload, ordering(&orders).expect("blobs")])
                .expect("two columns of a length");
            file.write(&chunk).expect("written");
        }
        file
    }

    /// Everything a merge of these runs gives back, in the order it gives it.
    fn merged(files: Vec<Runs>) -> Vec<i64> {
        let sorted = Sorted::new();
        sorted.merge(files, vec![LogicalType::BigInt]).expect("the runs open");
        let mut out = Vec::new();
        while let Some(mut morsel) = sorted.morsel() {
            loop {
                let mut chunk = Chunk::empty(&[]);
                let progress = sorted.read(&mut morsel, &mut chunk).expect("a chunk");
                for row in 0..chunk.len() {
                    match chunk.value_at(row, 0) {
                        Value::BigInt(value) => out.push(value),
                        other => panic!("a {other} came out of a BIGINT column"),
                    }
                }
                if progress == Progress::Done {
                    break;
                }
            }
        }
        out
    }

    #[test]
    fn two_runs_come_out_in_one_order() {
        let answer = merged(vec![run(0, &[1, 4, 7, 9], 2), run(1, &[2, 3, 8], 2)]);
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
        assert_eq!(merged(vec![run(1, &[5, 5], 4), run(0, &[5, 5], 4)]).len(), 4);
        assert!(order_of(&[0; KEY], (0, 0)) < order_of(&[0; KEY], (1, 0)));
        assert!(order_of(&[0; KEY], (0, 0)) < order_of(&[0; KEY], (0, 1)));
    }

    /// A run longer than a block, so the merge has to read the next chunk of it partway through.
    #[test]
    fn a_run_of_several_chunks_is_read_through() {
        let long: Vec<i64> = (0..20).map(|value| value * 2).collect();
        let short: Vec<i64> = (0..20).map(|value| value * 2 + 1).collect();
        let answer = merged(vec![run(0, &long, 3), run(1, &short, 7)]);
        assert_eq!(answer, (0..40).collect::<Vec<i64>>());
    }

    #[test]
    fn one_run_comes_back_as_itself() {
        assert_eq!(merged(vec![run(0, &[3, 4, 5], 2)]), vec![3, 4, 5]);
    }

    #[test]
    fn runs_with_nothing_in_them_merge_to_nothing() {
        assert!(merged(vec![run(0, &[], 2), run(1, &[], 2)]).is_empty());
    }

    /// Negative keys sort below positive ones, which is the flipped sign bit doing its job and is
    /// worth a test here because the merge never looks at the payload.
    #[test]
    fn the_key_bytes_order_the_way_the_numbers_do() {
        let answer = merged(vec![run(0, &[-9, -1, 3], 2), run(1, &[-5, 0, 7], 2)]);
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

    #[test]
    fn a_sort_that_has_not_finished_says_so_rather_than_answering_nothing() {
        let sorted = Sorted::new();
        let mut morsel = rudb_pipeline::Morsel::new(0, 0, 1);
        let mut out = Chunk::empty(&[]);
        let why = sorted.read(&mut morsel, &mut out).expect_err("nothing was handed over");
        assert!(why.to_string().contains("read before it finished"), "{why}");
    }
}
