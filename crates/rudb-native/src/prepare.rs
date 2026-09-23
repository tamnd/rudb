//! A stripe encoded before the writer is asked for it.
//!
//! A load fed by many pipeline instances has one [`Writer`] behind one lock, and until this every
//! instance encoded its stripe while holding that lock. On the 32 core box, loading the ClickBench
//! 10m sample spent 68% of all its processor time in the stripe encode, all of it under the lock,
//! and the instances waited 146 seconds between them for a load whose wall clock was 20.6 seconds.
//! The machine was one encode at a time with thirty one readers queued behind it.
//!
//! Almost none of that work needs the writer. A plain column's pages, its sieves, its ranges and
//! its statistics depend on the stripe's own rows and nothing else. The one thing a stripe shares
//! with the rest of the table is a varchar column's global dictionary, because a code has to mean
//! the same value in every page of the column. So a stripe is taken in four steps:
//!
//! 1. [`Preparer::prepare`], with no lock. Every column without a global dictionary is encoded to
//!    its pages, and every column with one is coded against a dictionary of the stripe's own, which
//!    holds each distinct value of the stripe once, in the order the rows first held it. The
//!    statistics of every column are folded into a gather of the stripe's own.
//! 2. [`Writer::merge`], under the lock. The stripe's dictionaries go into the global ones a
//!    distinct value at a time, which gives back what each local code is globally, and the gathers
//!    are absorbed. This is the only step that has to see the stripes one at a time. Every
//!    dictionary block the merge filled is taken out with the stripe.
//! 3. [`Merged::pages`], with no lock. The codes are turned into global ones and built into pages,
//!    and the dictionary blocks the merge took out are encoded.
//! 4. [`Writer::write`], under the lock. The dictionary blocks go back in order, and they and the
//!    pages go into the file.
//!
//! The dictionary blocks were encoded in the fourth step, under the lock, until the 10m ClickBench
//! load on the 32 core box was measured spending 2.7 of its 14 seconds there, on thirty two threads
//! spawned for it every stripe, with every instance queued behind them.
//!
//! A value merged in the order the stripe first held it gets the code it would have got had the
//! stripe been coded against the global dictionary row by row, because the rows before its first
//! appearance hold only values that were already merged. So a writer taking the four steps one
//! after the other writes the same bytes as one that coded every row against the global dictionary,
//! which is what [`Writer::flush_pending`] does.
//!
//! A stripe lets go of its rows at the end of the first step. What it carries from there on is its
//! pages, its stripe dictionaries and codes, and a few numbers a part, so the stripes queued for the
//! lock are a fraction of the size of the rows they came from. A column that loses its dictionary
//! after it was coded against one is rebuilt from the stripe dictionary, which holds every value
//! the rows did. Keeping the rows until the write instead took the 10m ClickBench load on the 32
//! core box from 3.5 GB resident to 9.9 GB, with thirty two stripes waiting at a time.

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering as Atomic};
use std::sync::{Arc, Mutex};

use rudb_common::{Error, LogicalType, Result};
use rudb_metrics::{LoadProfile, Stage};
use rudb_storage::Range;
use rudb_vector::{Bitmap, Chunk, Data, StringColumn, Validity, Vector};

use super::{
    ColumnStripe, DICTIONARY_CHECK_SEED, DICTIONARY_DECIDE_ROWS, DICTIONARY_DISTINCT_IN_TEN,
    EncodedBlock, GlobalDictionary, MAX_ENCODE_WORKERS, MAX_PAGE, Part, PendingChunk, STRIPE_PARTS,
    Spread, Unencoded, Writer, checksum, coded_page, invalid, push_validity, seeded_checksum,
    stats, unique_codes, weight,
};

/// How many stripes are being prepared or paged right now, across every writer in the process.
///
/// A stripe's columns are spread over threads of their own, which is what a writer being fed by
/// one caller needs, because that caller is the only one encoding. Thirty two callers each doing
/// that at once would be a thousand threads on a machine with thirty two cores. So each one takes
/// its share of the machine: the cores over however many stripes are being worked on right now.
static BUSY: AtomicUsize = AtomicUsize::new(0);

/// One stripe's share of the machine, held for as long as the stripe is being worked on.
struct Share(usize);

impl Share {
    fn take(columns: usize, parts: usize) -> Self {
        let busy = BUSY.fetch_add(1, Atomic::Relaxed) + 1;
        let cores =
            std::thread::available_parallelism().map_or(1, usize::from).min(MAX_ENCODE_WORKERS);
        // A stripe of one part is one small page a column, which is less than a thread is worth.
        let workers = if parts <= 1 { 1 } else { (cores / busy).clamp(1, columns.max(1)) };
        Self(workers)
    }
}

impl Drop for Share {
    fn drop(&mut self) {
        BUSY.fetch_sub(1, Atomic::Relaxed);
    }
}

/// Encodes stripes for one [`Writer`] without the writer.
///
/// Handed out by [`Writer::preparer`] and cheap to hold. It shares with its writer which varchar
/// columns still have a global dictionary, so a stripe prepared after the first one decided a
/// column should not have one is encoded plainly from the start.
#[derive(Debug, Clone)]
pub struct Preparer {
    types: Vec<LogicalType>,
    coded: Arc<[AtomicBool]>,
    profile: Option<Arc<LoadProfile>>,
}

/// A stripe that has been through [`Preparer::prepare`] and is waiting for [`Writer::merge`].
#[derive(Debug)]
pub struct Prepared {
    parts: Vec<Part>,
    types: Vec<LogicalType>,
    columns: Vec<Column>,
    gathers: Vec<Option<stats::Gather>>,
    profile: Option<Arc<LoadProfile>>,
}

/// A stripe that has been through [`Writer::merge`] and is waiting for [`Merged::pages`].
#[derive(Debug)]
pub struct Merged {
    parts: Vec<Part>,
    columns: Vec<Merge>,
    blocks: Vec<Unencoded>,
    profile: Option<Arc<LoadProfile>>,
    /// Whether the rows are counted into the table yet. [`Writer::merge`] counts them, and a
    /// [`Merger`] leaves them for [`Writer::write`], since it has no table to count them into.
    counted: bool,
}

/// A stripe that has been through [`Merged::pages`] and is waiting for [`Writer::write`].
#[derive(Debug)]
pub struct Paged {
    parts: Vec<Part>,
    columns: Vec<ColumnStripe>,
    /// Encoded dictionary blocks, each with its column and block number.
    blocks: Vec<(usize, usize, EncodedBlock)>,
    counted: bool,
}

/// What one job of [`Merged::pages`] built.
enum Built {
    Stripe(ColumnStripe),
    Block(EncodedBlock),
}

/// One column of a prepared stripe.
#[derive(Debug)]
enum Column {
    /// Finished, because the column has no global dictionary.
    Pages(ColumnStripe),
    /// Coded against the stripe's own dictionary, waiting to be merged into the global one.
    Coded(Local),
}

/// One column of a merged stripe.
#[derive(Debug)]
enum Merge {
    Pages(ColumnStripe),
    /// The local codes of every part, and the global code of every local one.
    Codes {
        parts: Vec<LocalPart>,
        global: Vec<u32>,
    },
    /// A column that was prepared against a dictionary it no longer has, which is every column
    /// prepared before the first stripe decided it should not have one. Encoded again, plainly,
    /// from the values its stripe dictionary holds.
    Plain(Local),
}

/// No value after this one has its hash.
const END: u32 = u32::MAX;

/// A dictionary of one column of one stripe.
///
/// The values are compared by their bytes rather than by a second hash, because they are all here
/// to compare. The global dictionary has two hashes to go on because its values are mostly in the
/// file by now. Both hashes are taken here, once a distinct value, so that merging it takes none.
#[derive(Debug, Default)]
struct Local {
    /// The first value holding each hash.
    first: HashMap<u64, u32, Spread>,
    /// The next value holding the same hash as this one, or [`END`].
    next: Vec<u32>,
    hashes: Vec<u64>,
    checks: Vec<u64>,
    /// The values back to back, and where each one ends.
    bytes: Vec<u8>,
    ends: Vec<usize>,
    /// How many rows that are not null hold each value, and how many are null.
    counts: Vec<u64>,
    nulls: u64,
    parts: Vec<LocalPart>,
}

/// One part of one column coded against its stripe's dictionary.
#[derive(Debug)]
struct LocalPart {
    codes: Vec<u32>,
    /// What [`push_validity`] wrote for the part, which is the page's second field onwards.
    validity: Vec<u8>,
    range: Range,
}

impl Local {
    /// One column of a stripe, coded.
    ///
    /// A null row is coded as the empty string and counted as a null rather than against it, which
    /// is what the writer has always done with one. The code is never read, since the page's
    /// validity says the row is null, and giving it one keeps the page one code a row.
    fn code_column(index: usize, held: &[PendingChunk]) -> Result<Self> {
        let mut local = Self::default();
        for pending in held {
            let column = pending.chunk.column(index)?;
            // flatten: the page is one code a row whatever form the rows came in.
            let flat = column.flatten()?;
            let mut codes = Vec::with_capacity(flat.len());
            let mut last = None;
            for row in 0..flat.len() {
                let text = flat.text_at(row).unwrap_or("").as_bytes();
                // A repeat of the row before is common enough on a sorted table to be worth a
                // comparison before a hash, and the comparison fails on its first bytes when not.
                let code = match last {
                    Some(code) if local.value(code) == text => code,
                    _ => local.code(text)?,
                };
                last = Some(code);
                if flat.is_null_at(row) {
                    local.nulls += 1;
                } else {
                    local.counts[code as usize] += 1;
                }
                codes.push(code);
            }
            let mut validity = Vec::new();
            push_validity(&mut validity, &flat);
            local.parts.push(LocalPart { codes, validity, range: Range::of(column) });
        }
        // Only the coding needs to find a value by its bytes, and on a column of URLs the table
        // that does it is as large as the codes.
        local.first = HashMap::default();
        local.next = Vec::new();
        Ok(local)
    }

    /// The column's parts as the rows they were coded from, for a column that lost its global
    /// dictionary after this stripe was coded against one.
    ///
    /// A null row comes back as a null over the empty string, which is what it was coded as, and
    /// each part gets back the same form of validity it had, since the page records which it was.
    fn rows(&self) -> Result<Vec<Vector>> {
        self.parts
            .iter()
            .map(|part| {
                let len = part.codes.len();
                let mut column = StringColumn::with_capacity(len);
                for &code in &part.codes {
                    column.push_bytes(self.value(code));
                }
                let validity = match part.validity.split_first() {
                    Some((0, _)) => Validity::AllValid,
                    Some((1, _)) => Validity::AllInvalid,
                    Some((2, bits)) => {
                        let mut mask = Bitmap::all_valid(len);
                        for row in (0..len).filter(|row| bits[row / 8] & (1 << (row % 8)) == 0) {
                            mask.set(row, false);
                        }
                        Validity::Mask(mask)
                    }
                    _ => return Err(Error::internal("a coded part has no validity")),
                };
                Ok(Vector::flat(LogicalType::Varchar, Data::Varlen(column))?
                    .with_validity(validity))
            })
            .collect()
    }

    fn values(&self) -> usize {
        self.ends.len()
    }

    fn value(&self, code: u32) -> &[u8] {
        let code = code as usize;
        let from = if code == 0 { 0 } else { self.ends[code - 1] };
        &self.bytes[from..self.ends[code]]
    }

    fn code(&mut self, text: &[u8]) -> Result<u32> {
        let hash = checksum(text);
        let Some(&first) = self.first.get(&hash) else {
            let code = self.push(text, hash)?;
            self.first.insert(hash, code);
            return Ok(code);
        };
        let mut at = first;
        loop {
            if self.value(at) == text {
                return Ok(at);
            }
            match self.next[at as usize] {
                END => break,
                next => at = next,
            }
        }
        let code = self.push(text, hash)?;
        self.next[at as usize] = code;
        Ok(code)
    }

    fn push(&mut self, text: &[u8], hash: u64) -> Result<u32> {
        let code = u32::try_from(self.ends.len())
            .ok()
            .filter(|&code| code != END)
            .ok_or_else(|| invalid("a stripe has too many values in one column"))?;
        self.bytes.extend_from_slice(text);
        self.ends.push(self.bytes.len());
        self.next.push(END);
        self.hashes.push(hash);
        self.checks.push(seeded_checksum(text, DICTIONARY_CHECK_SEED));
        self.counts.push(0);
        Ok(code)
    }

    /// Puts every value into `dictionary` in the order this stripe first held it, and says what
    /// each one's code is there.
    fn merge_into(&self, dictionary: &mut GlobalDictionary) -> Result<Vec<u32>> {
        let mut global = Vec::with_capacity(self.values());
        for (code, (&hash, &check)) in self.hashes.iter().zip(&self.checks).enumerate() {
            let text = self.value(code as u32);
            let at = dictionary.code_hashed(text, hash, check)?;
            let count = dictionary
                .counts
                .get_mut(at as usize)
                .ok_or_else(|| invalid("global dictionary count code is out of range"))?;
            *count = count.saturating_add(self.counts[code]);
            global.push(at);
        }
        dictionary.nulls = dictionary.nulls.saturating_add(self.nulls);
        Ok(global)
    }
}

/// Whether a column's first stripe says it should not have a global dictionary.
///
/// Every varchar column starts with one, because the writer cannot know what is in a column before
/// it has seen some of it. A global dictionary is the right shape for a column of a few dozen
/// values repeated down the table: the pages become small integers, a filter against a literal is
/// one search of the sorted order rather than a comparison a row, and a group by is on the codes.
/// It is the wrong shape for a column whose values are nearly all different. There the codes are
/// as wide as row numbers, nothing is saved on the pages, and the membership index of a stripe is
/// a list of very nearly every code in the column. On TPC-H the orders table written on its own
/// goes from 52.3 MB to 41.4 MB, the load from 6.9 s to 5.8 s, and `select o_comment from orders`
/// from 1.810 G instructions to 1.213 G, which is what the rudb parquet reader takes over the same
/// values.
///
/// So the first stripe of a column is the sample and the decision is made once on it. Once, rather
/// than per stripe, because the codes of one column have to mean the same thing in every page of
/// it, and a column that changed its mind halfway would need its earlier stripes rewritten. The
/// first stripe is encoded again when the answer comes out against the dictionary, which is the one
/// stripe that pays for the decision, along with any stripe that was prepared before it was made.
///
/// The threshold is deliberately near the top. [`DICTIONARY_DISTINCT_IN_TEN`] of the sample has to
/// be values never seen before, which is a column with essentially no repeats. Everything with real
/// repetition keeps its dictionary and keeps every property that hangs off it, and nothing is
/// claimed here about where between the two the crossover really sits.
fn drops_dictionary(rows: usize, distinct: usize) -> bool {
    rows >= DICTIONARY_DECIDE_ROWS
        && distinct.saturating_mul(10) > rows.saturating_mul(DICTIONARY_DISTINCT_IN_TEN)
}

/// Runs `work` on every one of `jobs`, spread over `workers` threads, and hands back each job with
/// what it came to, in no particular order.
///
/// The jobs are handed out through a queue rather than dealt in equal piles, because they are
/// nothing like equal: `URL` on ClickBench is a string column of sixty one million distinct values
/// and `IsMobile` is a byte. A pile that happened to hold the four large string columns would be
/// the whole stripe and the other workers would be waiting on it. The caller hands the jobs over
/// cheapest first and they are taken from the back, so the expensive ones go first, which is the
/// classic answer to a last job that runs longer than everything before it.
fn fan_out<T: Send>(
    jobs: Vec<usize>,
    workers: usize,
    profile: Option<&LoadProfile>,
    work: impl Fn(usize) -> Result<T> + Sync,
) -> Result<Vec<(usize, T)>> {
    if workers <= 1 || jobs.len() <= 1 {
        let _span = profile.map(|profile| profile.span(Stage::Pages));
        return jobs.into_iter().map(|index| Ok((index, work(index)?))).collect();
    }
    let workers = workers.min(jobs.len());
    let queue = Mutex::new(jobs);
    let pieces = std::thread::scope(|scope| {
        (0..workers)
            .map(|_| {
                scope.spawn(|| {
                    let _span = profile.map(|profile| profile.span(Stage::Pages));
                    let mut mine = Vec::new();
                    loop {
                        let taken = queue
                            .lock()
                            .map_err(|_| Error::internal("a native encode worker panicked"))?
                            .pop();
                        let Some(index) = taken else { break };
                        mine.push((index, work(index)?));
                    }
                    Ok(mine)
                })
            })
            .collect::<Vec<_>>()
            .into_iter()
            .map(|handle| {
                handle.join().map_err(|_| Error::internal("a native encode worker panicked"))?
            })
            .collect::<Result<Vec<Vec<_>>>>()
    })?;
    Ok(pieces.into_iter().flatten().collect())
}

/// One column of every part of a stripe.
fn column_of(held: &[PendingChunk], index: usize) -> Result<Vec<&Vector>> {
    held.iter().map(|pending| pending.chunk.column(index)).collect()
}

/// Puts what [`fan_out`] handed back in column order.
fn in_order<T>(width: usize, done: Vec<(usize, T)>) -> Result<Vec<T>> {
    let mut slots: Vec<Option<T>> = (0..width).map(|_| None).collect();
    for (index, one) in done {
        slots[index] = Some(one);
    }
    slots
        .into_iter()
        .map(|slot| slot.ok_or_else(|| Error::internal("a column was never encoded")))
        .collect()
}

impl Preparer {
    /// Encodes a run of chunks as one stripe, as far as it can be without the writer.
    ///
    /// The run is what [`Writer::append_stripe`] takes, and the rules are the same: it is a stripe
    /// of its own, and the orders have to come out in source order once the stripes are sorted. An
    /// empty chunk is dropped.
    ///
    /// # Errors
    ///
    /// If the run is longer than [`STRIPE_PARTS`], a chunk's columns are not the table's, or one
    /// cannot be encoded.
    pub fn prepare(&self, parts: Vec<((u64, u64), Chunk)>) -> Result<Prepared> {
        if parts.len() > STRIPE_PARTS {
            return Err(invalid("a stripe was handed more parts than it holds"));
        }
        let held = parts
            .into_iter()
            .filter(|(_, chunk)| !chunk.is_empty())
            .map(|(order, chunk)| PendingChunk { order, chunk })
            .collect::<Vec<_>>();
        for pending in &held {
            self.fits(&pending.chunk)?;
        }
        self.prepare_held(held)
    }

    /// The check [`Writer::admit`] makes, here because the rows are gone by the merge.
    fn fits(&self, chunk: &Chunk) -> Result<()> {
        if chunk.width() != self.types.len() {
            return Err(invalid("chunk width differs from table schema"));
        }
        for (index, ty) in self.types.iter().enumerate() {
            if chunk.column(index)?.logical_type() != ty {
                return Err(invalid("chunk type differs from table schema"));
            }
        }
        Ok(())
    }

    pub(crate) fn prepare_held(&self, held: Vec<PendingChunk>) -> Result<Prepared> {
        let width = self.types.len();
        let key = held.first().map_or((0, 0), |pending| pending.order);
        let share = Share::take(width, held.len());
        let mut jobs = (0..width).collect::<Vec<_>>();
        jobs.sort_by_key(|&index| weight(&self.types[index]));
        let done = fan_out(jobs, share.0, self.profile.as_deref(), |index| {
            // The statistics on the thread that is already walking the column, and in the same
            // step, because the rows are in memory once and this is the moment they are.
            let gather = stats::Gather::new(&self.types[index], 0)
                .filter(|_| !held.is_empty())
                .map(|mut gather| {
                    crate::probe::time(0, || {
                        gather.stripe(
                            key,
                            held.iter().filter_map(|pending| pending.chunk.column(index).ok()),
                        )
                    });
                    gather
                });
            let column = if self.coded[index].load(Atomic::Relaxed) {
                Column::Coded(crate::probe::time(1, || Local::code_column(index, &held))?)
            } else {
                Column::Pages(crate::probe::time(2, || {
                    Writer::encode_pages(&column_of(&held, index)?)
                })?)
            };
            Ok((column, gather))
        })?;
        drop(share);
        let (columns, gathers) = in_order(width, done)?.into_iter().unzip();
        let parts = held.iter().map(Part::of).collect();
        drop(held);
        Ok(Prepared {
            parts,
            types: self.types.clone(),
            columns,
            gathers,
            profile: self.profile.clone(),
        })
    }
}

/// Where one column's dictionary and statistics are while a stripe is merged into them.
enum Slot<'a> {
    /// In the writer, which the caller holds.
    Owned(&'a mut Option<GlobalDictionary>, &'a mut Option<stats::Gather>),
    /// Lent to a [`Merger`], behind the column's own lock.
    Lent(&'a Mutex<LentColumn>, &'a Lent),
}

/// One column of a stripe on its way through [`merge_columns`].
struct Step<'a> {
    index: usize,
    column: Column,
    slot: Slot<'a>,
    /// The stripe's statistics for the column, when the column keeps them.
    gather: Option<stats::Gather>,
}

impl Step<'_> {
    /// Roughly what the merge costs: a hash a distinct value when there is a global dictionary to
    /// merge into, and next to nothing otherwise. Read off `coded` rather than the dictionary, so a
    /// lent column does not have to be locked to be sorted.
    fn cost(&self, coded: &[AtomicBool]) -> usize {
        match &self.column {
            Column::Coded(local) if coded[self.index].load(Atomic::Relaxed) => {
                local.values().saturating_add(1)
            }
            _ => 0,
        }
    }

    /// Merges the column, settles its dictionary's shape and hands out the blocks it filled.
    fn run(self, rows: usize, coded: &[AtomicBool]) -> Result<(usize, Merge, Vec<Unencoded>)> {
        let Self { index, column, slot, gather } = self;
        match slot {
            Slot::Owned(dictionary, mine) => {
                merge_column(index, column, gather, dictionary, mine, rows, coded)
            }
            Slot::Lent(held, lent) => {
                let mut held = held.lock().map_err(|_| Error::internal("a merge panicked"))?;
                // Checked with the column locked, so a merge either finishes before the writer
                // takes this column back or is refused.
                if lent.reclaimed.load(Atomic::Acquire) {
                    return Err(Error::internal("a stripe was merged after its table was closed"));
                }
                let LentColumn { dictionary, gather: mine } = &mut *held;
                merge_column(index, column, gather, dictionary, mine, rows, coded)
            }
        }
    }
}

/// One column of [`merge_columns`].
fn merge_column(
    index: usize,
    column: Column,
    stripe: Option<stats::Gather>,
    dictionary: &mut Option<GlobalDictionary>,
    gather: &mut Option<stats::Gather>,
    rows: usize,
    coded: &[AtomicBool],
) -> Result<(usize, Merge, Vec<Unencoded>)> {
    if let (Some(mine), Some(stripe)) = (gather.as_mut(), stripe) {
        mine.absorb(stripe);
    }
    let merge_started = std::time::Instant::now();
    let merge = match (column, dictionary.as_mut()) {
        (Column::Pages(stripe), None) => Merge::Pages(stripe),
        (Column::Pages(_), Some(_)) => {
            return Err(Error::internal(
                "a column with a global dictionary was prepared without one",
            ));
        }
        (Column::Coded(local), None) => Merge::Plain(local),
        (Column::Coded(local), Some(global)) => {
            // Empty means nothing has been merged into it yet, so this is the column's first
            // stripe and the only one the decision is allowed to be made on.
            if global.values() == 0 && drops_dictionary(rows, local.values()) {
                *dictionary = None;
                coded[index].store(false, Atomic::Relaxed);
                Merge::Plain(local)
            } else {
                let global = local.merge_into(global)?;
                Merge::Codes { parts: local.parts, global }
            }
        }
    };
    // Settled here rather than when the stripe is written, so that the blocks this merge filled go
    // out with it already knowing their shape. A column still too small to settle one keeps its
    // blocks until it can, which is at most `PAYLOAD_SAMPLE_BLOCKS` of them, because encoding them
    // now would be encoding them without having looked at the column.
    crate::probe::add(6, merge_started);
    let blocks = match dictionary {
        Some(dictionary) => {
            crate::probe::time(7, || dictionary.settle())?;
            dictionary.hand_out(index)
        }
        None => Vec::new(),
    };
    Ok((index, merge, blocks))
}

/// Merges every column of a stripe into the dictionaries and statistics in `slots`.
///
/// Every column is merged on its own, because nothing one column's merge reads or writes belongs to
/// another: its statistics, its global dictionary and its flag in `coded`. So the columns are
/// spread over threads, and a stripe takes as long as its slowest column rather than all of them.
/// The answer is the same in any order, because a column's merge only depends on the stripes
/// merged into that column before it.
fn merge_columns(prepared: Prepared, slots: Vec<Slot<'_>>, coded: &[AtomicBool]) -> Result<Merged> {
    let Prepared { parts, columns, gathers, profile, .. } = prepared;
    let timing = profile.as_deref().map(|profile| profile.span(Stage::Dictionary));
    let rows: usize = parts.iter().map(|part| part.rows).sum();
    let width = columns.len();
    if slots.len() != width || gathers.len() != width {
        return Err(Error::internal("a stripe was merged into a table of another width"));
    }
    let mut steps = columns
        .into_iter()
        .zip(gathers)
        .zip(slots)
        .enumerate()
        .map(|(index, ((column, gather), slot))| Step { index, column, slot, gather })
        .collect::<Vec<_>>();
    // Taken from the back, so the biggest merges start first and the last one to finish is
    // small, the same reason `fan_out` hands its jobs over cheapest first.
    steps.sort_by_key(|step| step.cost(coded));
    let workers = std::thread::available_parallelism()
        .map_or(1, usize::from)
        .min(MAX_ENCODE_WORKERS)
        .min(steps.iter().filter(|step| step.cost(coded) > 0).count())
        .max(1);
    let done = if workers <= 1 {
        steps.into_iter().map(|step| step.run(rows, coded)).collect::<Result<Vec<_>>>()?
    } else {
        let queue = Mutex::new(steps);
        let pieces = std::thread::scope(|scope| {
            (0..workers)
                .map(|_| {
                    scope.spawn(|| {
                        let mut mine = Vec::new();
                        loop {
                            let taken = queue
                                .lock()
                                .map_err(|_| Error::internal("a merge worker panicked"))?
                                .pop();
                            let Some(step) = taken else { break };
                            mine.push(step.run(rows, coded)?);
                        }
                        Ok(mine)
                    })
                })
                .collect::<Vec<_>>()
                .into_iter()
                .map(|handle| {
                    handle.join().map_err(|_| Error::internal("a merge worker panicked"))?
                })
                .collect::<Result<Vec<Vec<_>>>>()
        })?;
        pieces.into_iter().flatten().collect()
    };
    let mut slots: Vec<Option<(Merge, Vec<Unencoded>)>> = (0..width).map(|_| None).collect();
    for (index, merge, blocks) in done {
        slots[index] = Some((merge, blocks));
    }
    let mut merged = Vec::with_capacity(width);
    let mut blocks = Vec::new();
    for slot in slots {
        let (merge, handed) = slot.ok_or_else(|| Error::internal("a column was never merged"))?;
        merged.push(merge);
        blocks.extend(handed);
    }
    drop(timing);
    Ok(Merged { parts, columns: merged, blocks, profile, counted: false })
}

/// The dictionaries and statistics of a table while a [`Merger`] has them, one lock a column.
#[derive(Debug)]
pub(crate) struct Lent {
    columns: Box<[Mutex<LentColumn>]>,
    /// Set when the writer takes them back, after which a merge is refused.
    reclaimed: AtomicBool,
}

/// One column of [`Lent`].
#[derive(Debug)]
pub(crate) struct LentColumn {
    pub(crate) dictionary: Option<GlobalDictionary>,
    gather: Option<stats::Gather>,
}

impl Lent {
    pub(crate) fn columns(&self) -> &[Mutex<LentColumn>] {
        &self.columns
    }

    /// Puts encoded blocks back into their dictionaries, each under its own column's lock.
    fn take_back(&self, blocks: Vec<(usize, usize, EncodedBlock)>) -> Result<()> {
        for (column, at, block) in blocks {
            self.columns
                .get(column)
                .ok_or_else(|| Error::internal("a dictionary block came back to no column"))?
                .lock()
                .map_err(|_| Error::internal("a merge panicked"))?
                .dictionary
                .as_mut()
                .ok_or_else(|| Error::internal("a dictionary block came back to no dictionary"))?
                .take_back(at, block)?;
        }
        Ok(())
    }

    /// Everything lent, handed back to the writer.
    #[allow(clippy::type_complexity)]
    pub(crate) fn reclaim(
        &self,
    ) -> Result<(Vec<Option<GlobalDictionary>>, Vec<Option<stats::Gather>>)> {
        self.reclaimed.store(true, Atomic::Release);
        let mut dictionaries = Vec::with_capacity(self.columns.len());
        let mut gathers = Vec::with_capacity(self.columns.len());
        for column in &self.columns {
            let mut held = column.lock().map_err(|_| Error::internal("a merge panicked"))?;
            dictionaries.push(held.dictionary.take());
            gathers.push(held.gather.take());
        }
        Ok((dictionaries, gathers))
    }
}

/// Merges prepared stripes into a writer's dictionaries and statistics without the writer.
///
/// Handed out by [`Writer::merger`]. With it, a load that shares one writer between many threads
/// holds the writer's lock only to write, and two stripes merge at once as long as they are on
/// different columns. A stripe merged here is written with [`Writer::write`] as usual, and that is
/// where its rows are counted in.
#[derive(Debug, Clone)]
pub struct Merger {
    lent: Arc<Lent>,
    types: Vec<LogicalType>,
    coded: Arc<[AtomicBool]>,
}

impl Merger {
    /// [`Writer::merge`], one column lock at a time instead of the writer.
    ///
    /// # Errors
    ///
    /// If the stripe was prepared for a table of other columns, or the table was closed.
    pub fn merge(&self, prepared: Prepared) -> Result<Merged> {
        if prepared.types != self.types {
            return Err(invalid("a stripe was prepared for a table of other columns"));
        }
        let slots = self.lent.columns.iter().map(|column| Slot::Lent(column, &self.lent)).collect();
        merge_columns(prepared, slots, &self.coded)
    }

    /// Puts a stripe's encoded dictionary blocks back, so that [`Writer::write`] does not wait on a
    /// column's lock while it holds its own.
    ///
    /// # Errors
    ///
    /// If a block comes back to a column without a dictionary, or comes back twice.
    pub fn give_back(&self, paged: &mut Paged) -> Result<()> {
        self.lent.take_back(std::mem::take(&mut paged.blocks))
    }
}

impl Merged {
    /// Builds the pages the merge left to build, which is every column coded against a global
    /// dictionary and every column that lost one after the stripe was prepared, and encodes the
    /// dictionary blocks the merge filled.
    ///
    /// # Errors
    ///
    /// If a column or a block cannot be encoded or a page comes out larger than a page may be.
    pub fn pages(self) -> Result<Paged> {
        let Self { parts, columns, blocks, profile, counted } = self;
        let width = columns.len();
        // The blocks go first so that they are taken last. One block is a thousand values, which is
        // less than any column of a stripe, and small jobs at the end are what keeps the last
        // worker from finishing long after the others.
        let mut jobs = (width..width + blocks.len())
            .chain((0..width).filter(|&index| !matches!(columns[index], Merge::Pages(_))))
            .collect::<Vec<_>>();
        // A column encoded again from its rows costs more than one whose codes only need building.
        jobs.sort_by_key(|&index| index < width && matches!(columns[index], Merge::Plain(_)));
        let share = Share::take(jobs.len(), parts.len());
        let built = fan_out(jobs, share.0, profile.as_deref(), |index| {
            let Some(column) = columns.get(index) else {
                return Ok(Built::Block(crate::probe::time(3, || blocks[index - width].encode())?));
            };
            Ok(Built::Stripe(match column {
                Merge::Codes { parts, global } => {
                    crate::probe::time(4, || code_pages(parts, global))?
                }
                Merge::Plain(local) => crate::probe::time(5, || {
                    Writer::encode_pages(&local.rows()?.iter().collect::<Vec<_>>())
                })?,
                Merge::Pages(_) => {
                    return Err(Error::internal("a finished column was queued to be built"));
                }
            }))
        })?;
        drop(share);
        let mut slots: Vec<Option<ColumnStripe>> = (0..width).map(|_| None).collect();
        let mut encoded = Vec::with_capacity(blocks.len());
        for (index, one) in built {
            match one {
                Built::Stripe(stripe) => slots[index] = Some(stripe),
                Built::Block(block) => {
                    let (column, at) = blocks[index - width].place();
                    encoded.push((column, at, block));
                }
            }
        }
        let columns = columns
            .into_iter()
            .zip(slots)
            .map(|(column, slot)| match (column, slot) {
                (Merge::Pages(stripe), _) | (_, Some(stripe)) => Ok(stripe),
                _ => Err(Error::internal("a column was never encoded")),
            })
            .collect::<Result<Vec<_>>>()?;
        Ok(Paged { parts, columns, blocks: encoded, counted })
    }
}

/// One column's parts as pages of global codes.
fn code_pages(parts: &[LocalPart], global: &[u32]) -> Result<ColumnStripe> {
    let mut stripe = ColumnStripe {
        pages: Vec::with_capacity(parts.len()),
        codes: Vec::with_capacity(parts.len()),
        sieves: Vec::with_capacity(parts.len()),
        ranges: Vec::with_capacity(parts.len()),
    };
    for part in parts {
        let codes = part
            .codes
            .iter()
            .map(|&code| global.get(code as usize).copied())
            .collect::<Option<Vec<_>>>()
            .ok_or_else(|| Error::internal("a stripe's code has no global code"))?;
        let bytes = coded_page(&codes, &part.validity)?;
        if bytes.len() > MAX_PAGE {
            return Err(invalid("column page exceeds the configured bound"));
        }
        stripe.pages.push(bytes);
        stripe.codes.push(Some(unique_codes(&codes)));
        // None, because the codes already give the stripe an exact membership index, and an
        // approximate one beside it would cost a hash of every string to answer a question that
        // is already answered.
        stripe.sieves.push(None);
        stripe.ranges.push(part.range.clone());
    }
    Ok(stripe)
}

impl Writer {
    /// Something that encodes stripes for this writer without holding it. See [`Preparer::prepare`].
    ///
    /// It carries the profile the writer has when it is asked for, so a writer that is going to be
    /// given one with [`Writer::with_profile`] should be given it first.
    #[must_use]
    pub fn preparer(&self) -> Preparer {
        Preparer {
            types: self.table.fields.iter().map(|field| field.ty.clone()).collect(),
            coded: Arc::clone(&self.coded),
            profile: self.profile.clone(),
        }
    }

    /// Takes a prepared stripe into the table's dictionaries and statistics and counts its rows in.
    ///
    /// This is the step that has to see the stripes one at a time, and it is a hash a distinct
    /// value of each varchar column rather than two a row. Whatever [`Writer::append_at`] left
    /// behind is written first as its own stripe, the same rule [`Writer::append_stripe`] has.
    ///
    /// # Errors
    ///
    /// If the stripe was prepared for a table of other columns, or the buffered stripe cannot be
    /// written.
    pub fn merge(&mut self, prepared: Prepared) -> Result<Merged> {
        self.flush_pending()?;
        if prepared.columns.len() != self.table.fields.len()
            || prepared.types.iter().ne(self.table.fields.iter().map(|field| &field.ty))
        {
            return Err(invalid("a stripe was prepared for a table of other columns"));
        }
        self.table.rows = prepared
            .parts
            .iter()
            .try_fold(self.table.rows, |rows, part| rows.checked_add(part.rows))
            .ok_or_else(|| invalid("row count overflow"))?;
        self.merge_held(prepared)
    }

    /// [`Writer::merge`] for a stripe whose rows are already counted in.
    ///
    /// Every column is merged on its own, because nothing one column's merge reads or writes
    /// belongs to another: its statistics, its global dictionary and its flag in `coded`. So the
    /// columns are spread over threads, and the lock is held for the slowest column rather than for
    /// all of them. On ClickBench `hits` the lock was busy 98% of a load and the merge was four
    /// fifths of that, while two thirds of the machine waited for it. The answer is the same in any
    /// order, because a column's merge only depends on the stripes merged into it before.
    pub(crate) fn merge_held(&mut self, prepared: Prepared) -> Result<Merged> {
        let slots = match &self.lent {
            Some(lent) => lent.columns.iter().map(|column| Slot::Lent(column, lent)).collect(),
            None => self
                .dictionaries
                .iter_mut()
                .zip(self.gathers.iter_mut())
                .map(|(dictionary, gather)| Slot::Owned(dictionary, gather))
                .collect::<Vec<_>>(),
        };
        let mut merged = merge_columns(prepared, slots, &self.coded)?;
        merged.counted = true;
        Ok(merged)
    }

    /// Hands the dictionaries and the statistics to a [`Merger`], so that stripes can be merged
    /// without this writer's lock.
    ///
    /// Whatever [`Writer::append_at`] left behind is written first, the same rule
    /// [`Writer::merge`] has. The writer takes them back when the table is closed.
    ///
    /// # Errors
    ///
    /// If the buffered stripe cannot be written.
    pub fn merger(&mut self) -> Result<Merger> {
        self.flush_pending()?;
        let lent = match &self.lent {
            Some(lent) => Arc::clone(lent),
            None => {
                let lent = Arc::new(Lent {
                    columns: std::mem::take(&mut self.dictionaries)
                        .into_iter()
                        .zip(std::mem::take(&mut self.gathers))
                        .map(|(dictionary, gather)| Mutex::new(LentColumn { dictionary, gather }))
                        .collect(),
                    reclaimed: AtomicBool::new(false),
                });
                self.lent = Some(Arc::clone(&lent));
                lent
            }
        };
        Ok(Merger {
            lent,
            types: self.table.fields.iter().map(|field| field.ty.clone()).collect(),
            coded: Arc::clone(&self.coded),
        })
    }

    /// Writes a stripe whose pages are built.
    ///
    /// # Errors
    ///
    /// If the stripe was built for a table of another width or cannot be written.
    pub fn write(&mut self, paged: Paged) -> Result<()> {
        self.write_paged(paged)
    }

    pub(crate) fn write_paged(&mut self, paged: Paged) -> Result<()> {
        let Paged { parts, columns, blocks, counted } = paged;
        if !counted {
            self.table.rows = parts
                .iter()
                .try_fold(self.table.rows, |rows, part| rows.checked_add(part.rows))
                .ok_or_else(|| invalid("row count overflow"))?;
        }
        if let Some(lent) = &self.lent {
            lent.take_back(blocks)?;
        } else {
            for (column, at, block) in blocks {
                self.dictionaries
                    .get_mut(column)
                    .and_then(Option::as_mut)
                    .ok_or_else(|| {
                        Error::internal("a dictionary block came back to no dictionary")
                    })?
                    .take_back(at, block)?;
            }
        }
        if parts.is_empty() {
            return self.place_blocks();
        }
        self.write_stripe(&parts, columns)
    }

    /// All four steps one after the other, for a caller with nobody to share the writer with.
    ///
    /// # Errors
    ///
    /// The same as [`Writer::merge`], [`Merged::pages`] and [`Writer::write`].
    pub fn append_prepared(&mut self, prepared: Prepared) -> Result<()> {
        let merged = self.merge(prepared)?;
        let paged = merged.pages()?;
        self.write(paged)
    }
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::path::PathBuf;
    use std::time::{SystemTime, UNIX_EPOCH};

    use rudb_common::{Field, Value};
    use rudb_vector::Vector;

    use super::*;
    use crate::Reader;

    const PART: usize = 1_000;

    fn path(label: &str) -> PathBuf {
        let stamp = SystemTime::now().duration_since(UNIX_EPOCH).expect("time advances").as_nanos();
        std::env::temp_dir()
            .join(format!("rudb-prepare-{label}-{}-{stamp}.rdb", std::process::id()))
    }

    fn fields() -> Vec<Field> {
        vec![
            Field::required("id", LogicalType::BigInt),
            Field::new("city", LogicalType::Varchar),
            Field::new("note", LogicalType::Varchar),
        ]
    }

    /// The value every row holds, so a test can check a row it reads back without keeping the rows.
    ///
    /// `city` repeats a handful of values and has a null every so often, which keeps its dictionary.
    /// `note` is different on every row but its nulls, which loses it on the first stripe.
    fn row(id: usize) -> [Value; 3] {
        let city = if id % 11 == 0 {
            Value::Null
        } else {
            Value::Varchar(format!("city {}", (id / 7) % 13))
        };
        let note = if id % 17 == 0 { Value::Null } else { Value::Varchar(format!("note {id}")) };
        [Value::BigInt(id as i64), city, note]
    }

    /// A run of `parts` chunks starting at part `first`, as a caller hands them to the writer.
    fn stripe(first: usize, parts: usize) -> Vec<((u64, u64), Chunk)> {
        (first..first + parts)
            .map(|part| {
                let rows = (part * PART..(part + 1) * PART).map(row).collect::<Vec<_>>();
                let column = |at: usize| {
                    let values = rows.iter().map(|row| row[at].clone()).collect::<Vec<_>>();
                    Vector::from_values(fields()[at].ty.clone(), &values).expect("a column")
                };
                let chunk = Chunk::new(vec![column(0), column(1), column(2)]).expect("a chunk");
                ((part as u64, 0), chunk)
            })
            .collect()
    }

    /// The runs the tests hand over, out of source order so that the stripes are sorted at commit.
    fn runs() -> Vec<Vec<((u64, u64), Chunk)>> {
        vec![stripe(5, 5), stripe(0, 5), stripe(10, 3)]
    }

    fn check(path: &PathBuf) {
        let reader = Reader::open(path).expect("reopen");
        assert_eq!(reader.parts(), 13);
        for part in 0..13 {
            let chunk = reader.read(part, &[0, 1, 2]).expect("a part");
            for at in [0, 17, PART - 1] {
                let want = row(part * PART + at);
                for (column, value) in want.iter().enumerate() {
                    assert_eq!(&chunk.value_at(at, column), value, "part {part} row {at}");
                }
            }
        }
    }

    /// Every stripe prepared before any of them is merged writes the file that handing the same
    /// runs to the writer one at a time writes, byte for byte.
    ///
    /// That is the claim the whole split rests on. The second and third stripes here are coded
    /// against a dictionary for `note`, which the first stripe to be merged then decides the column
    /// should not have, so they are encoded again without it. `city` keeps its dictionary and the
    /// later stripes' values go into it in the order the merges happen.
    #[test]
    fn stripes_prepared_before_any_is_merged_write_the_same_bytes_as_one_at_a_time() {
        let alone = path("alone");
        let mut writer = Writer::create(&alone, "t", fields()).expect("a file");
        for run in runs() {
            writer.append_stripe(run).expect("a stripe");
        }
        writer.finish().expect("commit");

        let split = path("split");
        let mut writer = Writer::create(&split, "t", fields()).expect("a file");
        let preparer = writer.preparer();
        let prepared = runs()
            .into_iter()
            .map(|run| preparer.prepare(run).expect("prepared"))
            .collect::<Vec<_>>();
        for one in prepared {
            writer.append_prepared(one).expect("a stripe");
        }
        assert!(!preparer.coded[2].load(Atomic::Relaxed), "note lost its dictionary");
        assert!(preparer.coded[1].load(Atomic::Relaxed), "city kept its dictionary");
        writer.finish().expect("commit");

        assert_eq!(fs::read(&alone).expect("read"), fs::read(&split).expect("read"));
        check(&split);
        fs::remove_file(alone).expect("remove");
        fs::remove_file(split).expect("remove");
    }

    /// Two stripes merged in one order and written in the other read back as the rows they held,
    /// which is what two instances sharing a writer do whenever the second one's pages are built
    /// first.
    #[test]
    fn stripes_written_in_another_order_than_they_were_merged_read_back() {
        let path = path("crossed");
        let mut writer = Writer::create(&path, "t", fields()).expect("a file");
        let preparer = writer.preparer();
        let mut merged = runs()
            .into_iter()
            .map(|run| writer.merge(preparer.prepare(run).expect("prepared")).expect("merged"))
            .map(|merged| merged.pages().expect("paged"))
            .collect::<Vec<_>>();
        merged.reverse();
        for paged in merged {
            writer.write(paged).expect("written");
        }
        writer.finish().expect("commit");
        check(&path);
        fs::remove_file(path).expect("remove");
    }

    /// Stripes merged through a [`Merger`] write the same bytes as the writer merging them itself,
    /// and their rows are counted in when they are written.
    #[test]
    fn stripes_merged_through_a_merger_write_the_same_bytes_as_the_writer() {
        let alone = path("alone-merger");
        let mut writer = Writer::create(&alone, "t", fields()).expect("a file");
        for run in runs() {
            writer.append_stripe(run).expect("a stripe");
        }
        writer.finish().expect("commit");

        let lent = path("lent");
        let mut writer = Writer::create(&lent, "t", fields()).expect("a file");
        let preparer = writer.preparer();
        let merger = writer.merger().expect("a merger");
        for run in runs() {
            let merged = merger.merge(preparer.prepare(run).expect("prepared")).expect("merged");
            let mut paged = merged.pages().expect("paged");
            merger.give_back(&mut paged).expect("given back");
            writer.write(paged).expect("written");
        }
        assert_eq!(writer.table.rows, 13 * PART);
        writer.finish().expect("commit");

        assert_eq!(fs::read(&alone).expect("read"), fs::read(&lent).expect("read"));
        check(&lent);
        fs::remove_file(alone).expect("remove");
        fs::remove_file(lent).expect("remove");
    }

    /// Stripes merged on several threads at once through one [`Merger`] and written in whatever
    /// order they finish read back as the rows they held.
    #[test]
    fn stripes_merged_on_several_threads_at_once_read_back() {
        let path = path("merged-at-once");
        let mut writer = Writer::create(&path, "t", fields()).expect("a file");
        let preparer = writer.preparer();
        let merger = writer.merger().expect("a merger");
        let writer = Mutex::new(writer);
        std::thread::scope(|scope| {
            for run in runs() {
                let (preparer, merger, writer) = (&preparer, &merger, &writer);
                scope.spawn(move || {
                    let merged =
                        merger.merge(preparer.prepare(run).expect("prepared")).expect("merged");
                    let mut paged = merged.pages().expect("paged");
                    merger.give_back(&mut paged).expect("given back");
                    writer.lock().expect("the writer").write(paged).expect("written");
                });
            }
        });
        writer.into_inner().expect("the writer").finish().expect("commit");
        check(&path);
        fs::remove_file(path).expect("remove");
    }

    /// A merge that comes after the table is closed is refused rather than merged into
    /// dictionaries nothing will write.
    #[test]
    fn a_merge_after_the_table_is_closed_is_refused() {
        let path = path("late");
        let mut writer = Writer::create(&path, "t", fields()).expect("a file");
        let preparer = writer.preparer();
        let merger = writer.merger().expect("a merger");
        writer.finish().expect("commit");
        let prepared = preparer.prepare(stripe(0, 2)).expect("prepared");
        assert!(merger.merge(prepared).is_err());
        fs::remove_file(path).expect("remove");
    }

    /// A chunk that is not the table's is refused when it reaches the writer, and the writer is not
    /// left counting its rows.
    #[test]
    fn a_stripe_of_another_table_is_refused_at_the_merge() {
        let path = path("refused");
        let mut writer = Writer::create(&path, "t", fields()).expect("a file");
        let other = Writer::create(path.with_extension("other"), "u", vec![fields().remove(0)])
            .expect("a file");
        let prepared = other.preparer().prepare(vec![]).expect("nothing to prepare");
        assert!(writer.merge(prepared).is_err());
        assert_eq!(writer.table.rows, 0);
        drop(other);
        fs::remove_file(path.with_extension("other")).expect("remove");
        fs::remove_file(path).expect("remove");
    }
}
