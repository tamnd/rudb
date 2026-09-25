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
use std::ops::Deref;
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering as Atomic};
use std::sync::{Arc, Mutex};

use rudb_common::{Error, LogicalType, Result};
use rudb_metrics::{LoadProfile, Stage};
use rudb_storage::Range;
use rudb_storage::sieve::Sieve;
use rudb_vector::{Bitmap, Chunk, Data, StringColumn, Validity, Vector};

use super::{
    ColumnStripe, DICTIONARY_CHECK_SEED, DICTIONARY_DECIDE_ROWS, DICTIONARY_DISTINCT_IN_TEN,
    EncodedBlock, GlobalDictionary, MAX_ENCODE_WORKERS, MAX_PAGE, Part, PendingChunk, STRIPE_PARTS,
    Settling, Spread, Unencoded, Writer, checksum, coded_page, invalid, push_validity,
    seeded_checksum, stats, unique_codes, weight,
};

/// How many stripes are being prepared or paged right now, across every writer in the process.
///
/// A stripe's columns are spread over threads of their own, which is what a writer being fed by
/// one caller needs, because that caller is the only one encoding. Thirty two callers each doing
/// that at once would be a thousand threads on a machine with thirty two cores. So each one takes
/// its share of the machine: the cores over however many stripes are being worked on right now.
static BUSY: AtomicUsize = AtomicUsize::new(0);

/// What all of a table's global dictionaries may hold at once before the fastest growing one is
/// demoted, from section 5.5 of the encoding spec.
///
/// Dictionaries are the one thing a load holds that grows with the table rather than with the
/// stripe. `hits` has tens of millions of distinct `URL`, `Title`, `Referer` and `SearchPhrase`
/// values, at forty five to seventy bytes each for the lookup alone, which is past the whole two
/// gigabyte bound on its own.
pub const DICTIONARY_CAP_BYTES: u64 = 512 * 1024 * 1024;

/// Which varchar columns still code against a global dictionary, and what their dictionaries hold
/// between them.
///
/// Shared by a writer with every [`Preparer`] and [`Merger`] it hands out. The flags are what a
/// stripe prepared later reads to decide whether to code a column at all, and the rest is what a
/// merge reads to decide whether its column should stop, see [`demotes`].
#[derive(Debug)]
pub(crate) struct Coding {
    flags: Box<[AtomicBool]>,
    /// What each column's dictionary grew by in the last stripe merged into it.
    growth: Box<[AtomicU64]>,
    /// What every dictionary held the last time it was merged into, added up.
    held: AtomicU64,
    cap: AtomicU64,
    /// The lowest distinct count ceiling a stripe of each column has reported, `u64::MAX` until one
    /// has.
    ///
    /// Every stripe prepared for a writer is merged into the writer's statistics, so a stripe's full
    /// sketch is one of the parts of the union, and no hash above its ceiling can be in the union's
    /// bottom k. A stripe started later drops those hashes rather than hashing them into a table
    /// that the union would trim them from anyway. Without it, every stripe filled a sketch from
    /// empty, and `Sketch::insert` was about 2% of a `lineitem` load's samples.
    ceilings: Box<[AtomicU64]>,
}

impl Coding {
    pub(crate) fn new(flags: impl IntoIterator<Item = bool>) -> Self {
        let flags = flags.into_iter().map(AtomicBool::new).collect::<Box<[_]>>();
        let growth = flags.iter().map(|_| AtomicU64::new(0)).collect();
        let ceilings = flags.iter().map(|_| AtomicU64::new(u64::MAX)).collect();
        Self {
            flags,
            growth,
            held: AtomicU64::new(0),
            cap: AtomicU64::new(DICTIONARY_CAP_BYTES),
            ceilings,
        }
    }

    /// Sets what the dictionaries may hold between them before one is demoted.
    pub(crate) fn cap(&self, bytes: u64) {
        self.cap.store(bytes, Atomic::Relaxed);
    }

    /// Moves what one dictionary is counted for from `before` to `now`, and hands back the new
    /// total.
    fn recount(&self, before: u64, now: u64) -> u64 {
        if now >= before {
            self.held.fetch_add(now - before, Atomic::Relaxed) + (now - before)
        } else {
            self.held.fetch_sub(before - now, Atomic::Relaxed).saturating_sub(before - now)
        }
    }

    /// Whether the column grew the most in the stripes last merged into each column.
    ///
    /// Read without a lock across columns that may be merging at the same moment, so it is about
    /// the last stripe or the one before it. That is close enough for choosing which column to
    /// stop: a column that grows fastest keeps doing so, and it is asked again next stripe. A column
    /// that did not grow at all is never the one, since stopping it would free nothing.
    fn grew_most(&self, index: usize) -> bool {
        let mine = self.growth[index].load(Atomic::Relaxed);
        mine > 0 && self.growth.iter().all(|other| other.load(Atomic::Relaxed) <= mine)
    }
}

impl Deref for Coding {
    type Target = [AtomicBool];

    fn deref(&self) -> &[AtomicBool] {
        &self.flags
    }
}

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
    coded: Arc<Coding>,
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
    /// Whether the column is a blob rather than a varchar, for building its rows back.
    blob: bool,
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
    /// The bytes the dictionary and the codes of the parts so far take up.
    fn held(&self) -> usize {
        // A hash table slot is its key, its value and one control byte.
        self.first.capacity() * (size_of::<u64>() + size_of::<u32>() + 1)
            + spilled(&self.next)
            + spilled(&self.hashes)
            + spilled(&self.checks)
            + spilled(&self.bytes)
            + spilled(&self.ends)
            + spilled(&self.counts)
            + spilled(&self.parts)
            + self
                .parts
                .iter()
                .map(|part| spilled(&part.codes) + spilled(&part.validity))
                .sum::<usize>()
    }

    /// One column of a stripe, coded in one go.
    #[cfg(test)]
    fn code_column(index: usize, held: &[PendingChunk]) -> Result<Self> {
        let mut local = Self::default();
        let mut mapped = None;
        for pending in held {
            local.code_part(pending.chunk.column(index)?, &mut mapped)?;
        }
        local.done();
        Ok(local)
    }

    /// One more part of a column of a stripe, coded.
    ///
    /// A null row is coded as the empty string and counted as a null rather than against it, which
    /// is what the writer has always done with one. The code is never read, since the page's
    /// validity says the row is null, and giving it one keeps the page one code a row.
    ///
    /// The parts come to [`Local::code_part`] one at a time, in order, and [`Local::done`] ends the
    /// column, so a stripe can be coded as its parts arrive rather than once they are all held.
    fn code_part(
        &mut self,
        column: &Vector,
        mapped: &mut Option<(Arc<Vector>, Vec<u32>)>,
    ) -> Result<()> {
        self.blob = column.logical_type() == &LogicalType::Blob;
        if let Some(codes) = self.code_dictionary(column, mapped)? {
            let mut validity = Vec::new();
            push_validity(&mut validity, column);
            self.parts.push(LocalPart { codes, validity, range: Range::of(column) });
            return Ok(());
        }
        // flatten: the page is one code a row whatever form the rows came in.
        let flat = column.flatten()?;
        let mut codes = Vec::with_capacity(flat.len());
        let mut last = None;
        for row in 0..flat.len() {
            // bytes_at rather than text_at: the rows were checked for UTF-8 when they came in,
            // and checking every one again cost more than coding it.
            let text = flat.bytes_at(row).unwrap_or(b"");
            // A repeat of the row before is common enough on a sorted table to be worth a
            // comparison before a hash, and the comparison fails on its first bytes when not.
            let code = match last {
                Some(code) if self.value(code) == text => code,
                _ => self.code(text)?,
            };
            last = Some(code);
            if flat.is_null_at(row) {
                self.nulls += 1;
            } else {
                self.counts[code as usize] += 1;
            }
            codes.push(code);
        }
        let mut validity = Vec::new();
        push_validity(&mut validity, &flat);
        self.parts.push(LocalPart { codes, validity, range: Range::of(column) });
        Ok(())
    }

    /// Ends a column once its last part is coded.
    fn done(&mut self) {
        // Only the coding needs to find a value by its bytes, and on a column of URLs the table
        // that does it is as large as the codes.
        self.first = HashMap::default();
        self.next = Vec::new();
    }

    /// Codes a part that came in as codes into a dictionary of its own, which is how a Parquet page
    /// written with a dictionary arrives, by coding each value of that dictionary once rather than
    /// each row.
    ///
    /// Every row then costs a lookup in `mapped`, which holds the local code of each value of the
    /// last dictionary seen by its position in it, and a value is coded the first time a row holds
    /// it. So the codes come out in the order the rows first held each value, the same as coding the
    /// rows one at a time, and the stripe is the same stripe either way. The parts of one Parquet
    /// column chunk share their dictionary, so `mapped` carries over from one part to the next
    /// while it is the same one, found by the pointer and kept alive by holding it.
    ///
    /// `None` for anything else, and for a dictionary with a null in it, since a null row there is
    /// found through the value it points at and not through the part's own validity, which is the
    /// one [`push_validity`] writes.
    fn code_dictionary(
        &mut self,
        column: &Vector,
        mapped: &mut Option<(Arc<Vector>, Vec<u32>)>,
    ) -> Result<Option<Vec<u32>>> {
        let Some((codes, values)) = column.shared_dictionary_parts() else { return Ok(None) };
        if !matches!(values.validity(), Validity::AllValid) {
            return Ok(None);
        }
        let Some(codes) = codes.get(..column.len()) else { return Ok(None) };
        let fresh = !matches!(mapped, Some((held, _)) if Arc::ptr_eq(held, values));
        if fresh {
            *mapped = Some((Arc::clone(values), vec![END; values.len()]));
        }
        let Some((_, map)) = mapped.as_mut() else { return Ok(None) };
        let every = matches!(column.validity(), Validity::AllValid);
        let mut coded = Vec::with_capacity(codes.len());
        for (row, &code) in codes.iter().enumerate() {
            if !every && !column.validity().is_valid(row) {
                // Coded as the empty string and counted as a null, as `code_part` does.
                let code = self.code(b"")?;
                self.nulls += 1;
                coded.push(code);
                continue;
            }
            let slot = map
                .get_mut(code as usize)
                .ok_or_else(|| invalid("a dictionary code is out of range"))?;
            if *slot == END {
                *slot = self.code(values.bytes_at(code as usize).unwrap_or(b""))?;
            }
            self.counts[*slot as usize] += 1;
            coded.push(*slot);
        }
        Ok(Some(coded))
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
                let ty = if self.blob { LogicalType::Blob } else { LogicalType::Varchar };
                Ok(Vector::flat(ty, Data::Varlen(column))?.with_validity(validity))
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

/// Whether a column's dictionary should stop taking values after a stripe that added `new` of them
/// in `rows` rows, with the dictionaries holding `total` bytes between them.
///
/// Section 5.5 of the encoding spec, which asks at every stripe what [`drops_dictionary`] asks at
/// the first. A column that turns into a column of new values partway through, which is what a
/// URL column of a log does once its first hours are past, stops growing its dictionary one stripe
/// after it turns rather than at the end of the load. The stripes already coded keep their codes,
/// so unlike the first stripe's decision this one costs nothing to make late.
///
/// The second reason is the cap. Once the dictionaries together hold more than it, the column that
/// grew the most in its last stripe is the one that stops, because it is the one that would have
/// taken the most of what is left.
fn demotes(rows: usize, new: usize, total: u64, coding: &Coding, index: usize) -> bool {
    drops_dictionary(rows, new)
        || (total > coding.cap.load(Atomic::Relaxed) && coding.grew_most(index))
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
        let mut building = self.start();
        self.feed_held(&mut building, held)?;
        self.finish(building)
    }

    /// Starts a stripe that will be handed over a few parts at a time.
    ///
    /// [`Preparer::prepare`] takes a stripe whole, which means a caller holds every part of it
    /// decoded until the last one arrives, and every load instance holds one. Here each batch is
    /// encoded as it comes and let go, so what an instance holds is one batch of rows and what it
    /// has built from the batches before. The stripe comes out the same: the parts of a column go through
    /// the same steps in the same order, only with the rows of later parts not yet in memory.
    ///
    /// Whether a column is coded against its global dictionary is read here, once, so every part of
    /// the stripe is encoded the same way even if the column is demoted while it is being built.
    /// A stripe that ends up coded against a dictionary its column no longer has is encoded again
    /// plainly at the merge, which is what happens to a stripe prepared whole at the same moment.
    #[must_use]
    pub fn start(&self) -> Building {
        let columns = (0..self.types.len())
            .map(|index| {
                let body = if self.coded[index].load(Atomic::Relaxed) {
                    Body::Coded(Local::default(), None)
                } else {
                    Body::Pages(ColumnStripe::default(), Settling::default())
                };
                let mut gather = stats::Gather::new(&self.types[index], 0);
                let ceiling = self.coded.ceilings[index].load(Atomic::Relaxed);
                if let Some(gather) = gather.as_mut().filter(|_| ceiling < u64::MAX) {
                    gather.cap_at(ceiling);
                }
                Mutex::new(Growing { body, gather })
            })
            .collect();
        Building { parts: Vec::new(), columns }
    }

    /// Encodes the next few parts of a stripe that was started with [`Preparer::start`].
    ///
    /// The same rules as [`Preparer::prepare`]: the orders come in source order, an empty chunk is
    /// dropped, and the stripe holds no more than [`STRIPE_PARTS`] in all.
    ///
    /// # Errors
    ///
    /// If the stripe would hold too many parts, a chunk's columns are not the table's, or one cannot
    /// be encoded.
    pub fn feed(&self, building: &mut Building, parts: Vec<((u64, u64), Chunk)>) -> Result<()> {
        if building.parts.len().saturating_add(parts.len()) > STRIPE_PARTS {
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
        self.feed_held(building, held)
    }

    fn feed_held(&self, building: &mut Building, held: Vec<PendingChunk>) -> Result<()> {
        if held.is_empty() {
            return Ok(());
        }
        let width = self.types.len();
        // The stripe's key is its first part's order, and its statistics open with it.
        let opening = building.parts.is_empty();
        let key = held.first().map_or((0, 0), |pending| pending.order);
        let share = Share::take(width, held.len());
        let mut jobs = (0..width).collect::<Vec<_>>();
        jobs.sort_by_key(|&index| weight(&self.types[index]));
        let columns = &building.columns;
        fan_out(jobs, share.0, self.profile.as_deref(), |index| {
            let mut growing = columns[index]
                .lock()
                .map_err(|_| Error::internal("a native encode worker panicked"))?;
            let Growing { body, gather } = &mut *growing;
            if matches!(body, Body::Coded(..)) && !self.coded[index].load(Atomic::Relaxed) {
                body.plain()?;
            }
            if let Some(gather) = gather.as_mut() {
                // The statistics on the thread that is already walking the column, and in the same
                // step, because the rows are in memory once and this is the moment they are.
                if opening {
                    gather.open_stripe(key);
                }
                for pending in &held {
                    gather.part(pending.chunk.column(index)?);
                }
            }
            match body {
                Body::Coded(local, mapped) => {
                    for pending in &held {
                        local.code_part(pending.chunk.column(index)?, mapped)?;
                    }
                }
                Body::Pages(stripe, settling) => {
                    for pending in &held {
                        Writer::encode_page(stripe, settling, pending.chunk.column(index)?)?;
                    }
                }
            }
            Ok(())
        })?;
        drop(share);
        building.parts.extend(held.iter().map(Part::of));
        Ok(())
    }

    /// Ends a stripe that was started with [`Preparer::start`], ready for the merge.
    ///
    /// # Errors
    ///
    /// If a column's worker panicked.
    pub fn finish(&self, building: Building) -> Result<Prepared> {
        let empty = building.parts.is_empty();
        let (columns, gathers) = building
            .columns
            .into_iter()
            .enumerate()
            .map(|(index, growing)| {
                let Growing { body, gather } = growing
                    .into_inner()
                    .map_err(|_| Error::internal("a native encode worker panicked"))?;
                let column = match body {
                    Body::Coded(mut local, _) => {
                        local.done();
                        Column::Coded(local)
                    }
                    Body::Pages(stripe, _) => Column::Pages(stripe),
                };
                // A stripe of no parts has no statistics, rather than an empty stripe of them.
                let gather = gather.filter(|_| !empty).map(|mut gather| {
                    gather.close_stripe();
                    if let Some(ceiling) = gather.ceiling() {
                        self.coded.ceilings[index].fetch_min(ceiling, Atomic::Relaxed);
                    }
                    gather
                });
                Ok((column, gather))
            })
            .collect::<Result<Vec<_>>>()?
            .into_iter()
            .unzip();
        Ok(Prepared {
            parts: building.parts,
            types: self.types.clone(),
            columns,
            gathers,
            profile: self.profile.clone(),
        })
    }
}

/// A stripe that [`Preparer::start`] began and [`Preparer::feed`] is adding parts to.
#[derive(Debug)]
pub struct Building {
    parts: Vec<Part>,
    /// Behind a lock each so a column can be handed to whichever worker takes it. Only one does
    /// at a time, so the locks are never waited on.
    columns: Vec<Mutex<Growing>>,
}

impl Building {
    /// How many parts the stripe holds so far.
    #[must_use]
    pub fn parts(&self) -> usize {
        self.parts.len()
    }

    /// The bytes the stripe holds so far: every column's codes, stripe dictionary and pages.
    ///
    /// The rows a load hands in are charged until they are encoded, and this is what they turn
    /// into. It is not the small fraction of them it sounds like. A text column coded against its
    /// stripe dictionary keeps four bytes of code a row and every distinct value with its hashes,
    /// and each worker of a load has a stripe of its own going.
    #[must_use]
    pub fn held(&self) -> u64 {
        let bytes: usize = self
            .columns
            .iter()
            .map(|growing| {
                growing.lock().map_or(0, |growing| match &growing.body {
                    Body::Pages(stripe, _) => stripe_bytes(stripe),
                    Body::Coded(local, mapped) => {
                        local.held() + mapped.as_ref().map_or(0, |(_, codes)| spilled(codes))
                    }
                })
            })
            .sum();
        bytes as u64
    }
}

/// The bytes a column's finished pages and what goes beside them take up.
fn stripe_bytes(stripe: &ColumnStripe) -> usize {
    stripe.pages.iter().map(Vec::capacity).sum::<usize>()
        + spilled(&stripe.pages)
        + spilled(&stripe.sums)
        + stripe.codes.iter().flatten().map(spilled).sum::<usize>()
        + spilled(&stripe.codes)
        + stripe.sieves.iter().flatten().map(Sieve::len).sum::<usize>()
        + spilled(&stripe.sieves)
        + spilled(&stripe.ranges)
}

/// The bytes a vector's buffer takes up, whatever is in it.
fn spilled<T>(values: &Vec<T>) -> usize {
    values.capacity() * size_of::<T>()
}

/// One column of a stripe that is being built.
#[derive(Debug)]
struct Growing {
    body: Body,
    gather: Option<stats::Gather>,
}

/// What one column of a stripe being built has come to so far.
#[derive(Debug)]
enum Body {
    /// Pages, with what the parts so far have settled on.
    Pages(ColumnStripe, Settling),
    /// Codes against the stripe's own dictionary, with the Parquet dictionary the last part came
    /// in, as [`Local::code_dictionary`] keeps it.
    Coded(Local, Option<(Arc<Vector>, Vec<u32>)>),
}

impl Body {
    /// Turns a column coded against its dictionary into pages, for a column that lost its
    /// dictionary while the stripe was being built.
    ///
    /// The merge would encode the whole stripe again plainly once it saw the column had none. Doing
    /// it here, on the parts coded so far, means the parts still to come are encoded once. A load
    /// starts every stripe it has in flight before the first one reaches the merge and decides.
    fn plain(&mut self) -> Result<()> {
        let Self::Coded(local, _) = self else { return Ok(()) };
        let mut stripe = ColumnStripe::default();
        let mut settling = Settling::default();
        for rows in local.rows()? {
            Writer::encode_page(&mut stripe, &mut settling, &rows)?;
        }
        *self = Self::Pages(stripe, settling);
        Ok(())
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
    fn cost(&self, coded: &Coding) -> usize {
        match &self.column {
            Column::Coded(local) if coded[self.index].load(Atomic::Relaxed) => {
                local.values().saturating_add(1)
            }
            _ => 0,
        }
    }

    /// Merges the column, settles its dictionary's shape and hands out the blocks it filled.
    fn run(
        self,
        rows: usize,
        coded: &Coding,
        profile: Option<&LoadProfile>,
    ) -> Result<(usize, Merge, Vec<Unencoded>)> {
        let Self { index, column, slot, gather } = self;
        match slot {
            Slot::Owned(dictionary, mine) => {
                merge_column(index, column, gather, dictionary, mine, rows, coded, profile)
            }
            Slot::Lent(held, lent) => {
                let mut held = held.lock().map_err(|_| Error::internal("a merge panicked"))?;
                // Checked with the column locked, so a merge either finishes before the writer
                // takes this column back or is refused.
                if lent.reclaimed.load(Atomic::Acquire) {
                    return Err(Error::internal("a stripe was merged after its table was closed"));
                }
                let LentColumn { dictionary, gather: mine } = &mut *held;
                merge_column(index, column, gather, dictionary, mine, rows, coded, profile)
            }
        }
    }
}

/// One column of [`merge_columns`].
#[expect(clippy::too_many_arguments, reason = "one column's share of the stripe's merge state")]
fn merge_column(
    index: usize,
    column: Column,
    stripe: Option<stats::Gather>,
    dictionary: &mut Option<GlobalDictionary>,
    gather: &mut Option<stats::Gather>,
    rows: usize,
    coded: &Coding,
    profile: Option<&LoadProfile>,
) -> Result<(usize, Merge, Vec<Unencoded>)> {
    if let (Some(mine), Some(stripe)) = (gather.as_mut(), stripe) {
        mine.absorb(stripe);
    }
    let mut new = None;
    let merge = match (column, dictionary.as_mut()) {
        (Column::Pages(stripe), None) => Merge::Pages(stripe),
        (Column::Pages(stripe), Some(global)) if global.demoted => Merge::Pages(stripe),
        (Column::Pages(_), Some(_)) => {
            return Err(Error::internal(
                "a column with a global dictionary was prepared without one",
            ));
        }
        (Column::Coded(local), None) => Merge::Plain(local),
        // Prepared before the column was demoted and merged after.
        (Column::Coded(local), Some(global)) if global.demoted => Merge::Plain(local),
        (Column::Coded(local), Some(global)) => {
            // Empty means nothing has been merged into it yet, so this is the column's first
            // stripe and the only one the decision is allowed to be made on.
            if global.values() == 0 && drops_dictionary(rows, local.values()) {
                if let Some(profile) = profile {
                    profile.release(global.charged);
                }
                coded.recount(global.charged, 0);
                *dictionary = None;
                coded[index].store(false, Atomic::Relaxed);
                Merge::Plain(local)
            } else {
                let before = global.values();
                let codes = local.merge_into(global)?;
                new = Some(global.values() - before);
                Merge::Codes { parts: local.parts, global: codes }
            }
        }
    };
    // Asked before the blocks go out, so that a demotion's sealed part block goes out with them.
    if let (Some(new), Some(global)) = (new, dictionary.as_mut()) {
        let now = global.held_bytes();
        coded.growth[index].store(now.saturating_sub(global.charged), Atomic::Relaxed);
        let total =
            coded.held.load(Atomic::Relaxed).saturating_add(now).saturating_sub(global.charged);
        if demotes(rows, new, total, coded, index) {
            global.demote();
            coded[index].store(false, Atomic::Relaxed);
            coded.growth[index].store(0, Atomic::Relaxed);
        }
    }
    // Settled here rather than when the stripe is written, so that the blocks this merge filled go
    // out with it already knowing their shape. A column still too small to settle one keeps its
    // blocks until it can, which is at most `PAYLOAD_SAMPLE_BLOCKS` of them, because encoding them
    // now would be encoding them without having looked at the column.
    let blocks = match dictionary {
        Some(dictionary) => {
            dictionary.settle()?;
            let blocks = dictionary.hand_out(index);
            let (before, now) = dictionary.recharge(profile);
            coded.recount(before, now);
            blocks
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
fn merge_columns(prepared: Prepared, slots: Vec<Slot<'_>>, coded: &Coding) -> Result<Merged> {
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
        steps
            .into_iter()
            .map(|step| step.run(rows, coded, profile.as_deref()))
            .collect::<Result<Vec<_>>>()?
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
                            mine.push(step.run(rows, coded, profile.as_deref())?);
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
    coded: Arc<Coding>,
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
                return Ok(Built::Block(blocks[index - width].encode()?));
            };
            Ok(Built::Stripe(match column {
                Merge::Codes { parts, global } => code_pages(parts, global)?,
                Merge::Plain(local) => {
                    Writer::encode_pages(&local.rows()?.iter().collect::<Vec<_>>())?
                }
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
        sums: Vec::with_capacity(parts.len()),
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
        stripe.sums.push(checksum(&bytes));
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

    /// A stripe that came in as codes into Parquet style dictionaries codes to the same stripe as
    /// the same rows would flat: the same values in the same order, the same codes, counts, nulls
    /// and validity. Two of the parts share a dictionary and one has one of its own, and one of the
    /// shared ones has nulls pointing at a value that is not the empty string. None of them is
    /// flattened on the way.
    #[test]
    fn dictionary_parts_code_as_their_rows_would() {
        let texts = |values: &[&str]| {
            Arc::new(
                Vector::from_values(
                    LogicalType::Varchar,
                    &values
                        .iter()
                        .map(|text| Value::Varchar((*text).to_string()))
                        .collect::<Vec<_>>(),
                )
                .expect("a dictionary"),
            )
        };
        let shared = texts(&["b", "a", "", "c", "unused"]);
        let other = texts(&["c", "d", "a"]);
        let mut nulls = Bitmap::all_valid(6);
        nulls.set(1, false);
        nulls.set(4, false);
        let parts = [
            Vector::dictionary_over(vec![3, 3, 1, 0, 2, 1], Arc::clone(&shared)).expect("codes"),
            Vector::dictionary_over(vec![0, 3, 1, 1, 3, 2], Arc::clone(&shared))
                .expect("codes")
                .with_validity(Validity::Mask(nulls)),
            Vector::dictionary_over(vec![1, 2, 0, 1], other).expect("codes"),
        ];
        let held = |flat: bool| {
            parts
                .iter()
                .enumerate()
                .map(|(at, part)| PendingChunk {
                    order: (at as u64, 0),
                    chunk: Chunk::new(vec![if flat {
                        part.flatten().expect("flat")
                    } else {
                        part.clone()
                    }])
                    .expect("a chunk"),
                })
                .collect::<Vec<_>>()
        };
        let parquet = held(false);
        let before = rudb_common::slow::here();
        let coded = Local::code_column(0, &parquet).expect("coded");
        assert_eq!(
            rudb_common::slow::here().since(before).get(rudb_common::slow::Cause::Flatten),
            0,
            "a part that came in as codes was flattened",
        );
        let flat = Local::code_column(0, &held(true)).expect("coded");
        assert_eq!(coded.values(), flat.values());
        for code in 0..flat.values() as u32 {
            assert_eq!(coded.value(code), flat.value(code), "value {code}");
        }
        assert_eq!(coded.counts, flat.counts);
        assert_eq!(coded.nulls, flat.nulls);
        assert_eq!(coded.nulls, 2);
        assert_eq!(coded.hashes, flat.hashes);
        assert_eq!(coded.checks, flat.checks);
        assert_eq!(coded.parts.len(), flat.parts.len());
        for (coded, flat) in coded.parts.iter().zip(&flat.parts) {
            assert_eq!(coded.codes, flat.codes);
            assert_eq!(coded.validity, flat.validity);
        }
    }

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

    /// Stripes counted under the ceiling an earlier stripe reported come to the same distinct count
    /// as stripes counted from nothing, whatever order they are merged in.
    ///
    /// The file cannot say this on its own, because a table this small may not be given the budget
    /// for its sketches, so the writer's statistics are compared before it closes.
    #[test]
    fn stripes_counted_under_an_earlier_ceiling_count_what_they_would_have() {
        let estimates = |writer: &Writer| {
            writer
                .gathers
                .iter()
                .map(|gather| gather.as_ref().and_then(stats::Gather::distinct))
                .collect::<Vec<_>>()
        };
        // What one gather makes of every row, with no stripe and so no ceiling anywhere.
        let want = fields()
            .iter()
            .enumerate()
            .map(|(column, field)| {
                let mut gather = stats::Gather::new(&field.ty, 0)?;
                for (_, chunk) in runs().into_iter().flatten() {
                    gather.part(chunk.column(column).expect("a column"));
                }
                gather.distinct()
            })
            .collect::<Vec<_>>();

        for reversed in [false, true] {
            let split = path("capped");
            let mut writer = Writer::create(&split, "t", fields()).expect("a file");
            let preparer = writer.preparer();
            let mut prepared = runs()
                .into_iter()
                .map(|run| preparer.prepare(run).expect("prepared"))
                .collect::<Vec<_>>();
            for column in [0, 2] {
                assert!(preparer.coded.ceilings[column].load(Atomic::Relaxed) < u64::MAX);
            }
            if reversed {
                prepared.reverse();
            }
            for one in prepared {
                writer.append_prepared(one).expect("a stripe");
            }
            assert_eq!(estimates(&writer), want, "reversed {reversed}");
            writer.finish().expect("commit");
            fs::remove_file(split).expect("remove");
        }
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

    /// A stripe fed a few parts at a time, with an empty batch and an empty chunk among them,
    /// writes the same bytes as the same stripe prepared whole.
    #[test]
    fn stripes_fed_in_batches_write_the_same_bytes_as_prepared_whole() {
        let whole = path("whole");
        let mut writer = Writer::create(&whole, "t", fields()).expect("a file");
        for run in runs() {
            writer.append_stripe(run).expect("a stripe");
        }
        writer.finish().expect("commit");

        let fed = path("fed");
        let mut writer = Writer::create(&fed, "t", fields()).expect("a file");
        let preparer = writer.preparer();
        let merger = writer.merger().expect("a merger");
        let nothing = Chunk::new(
            fields()
                .iter()
                .map(|field| Vector::from_values(field.ty.clone(), &[]).expect("a column"))
                .collect(),
        )
        .expect("a chunk");
        for mut run in runs() {
            let mut building = preparer.start();
            preparer.feed(&mut building, Vec::new()).expect("fed nothing");
            while !run.is_empty() {
                let rest = run.split_off(2.min(run.len()));
                let mut batch = std::mem::replace(&mut run, rest);
                batch.push(((u64::MAX, 0), nothing.clone()));
                preparer.feed(&mut building, batch).expect("fed");
            }
            let merged =
                merger.merge(preparer.finish(building).expect("finished")).expect("merged");
            let mut paged = merged.pages().expect("paged");
            merger.give_back(&mut paged).expect("given back");
            writer.write(paged).expect("written");
        }
        writer.finish().expect("commit");

        assert_eq!(fs::read(&whole).expect("read"), fs::read(&fed).expect("read"));
        check(&fed);
        fs::remove_file(whole).expect("remove");
        fs::remove_file(fed).expect("remove");
    }

    /// A stripe started while `note` still had its dictionary, which the first stripe's merge then
    /// dropped, encodes the rest of `note` as pages and reads back.
    #[test]
    fn a_column_dropped_while_its_stripe_is_built_turns_to_pages() {
        let path = path("dropped-while-built");
        let mut writer = Writer::create(&path, "t", fields()).expect("a file");
        let preparer = writer.preparer();
        let merger = writer.merger().expect("a merger");
        let write = |writer: &mut Writer, building: Building| {
            let merged =
                merger.merge(preparer.finish(building).expect("finished")).expect("merged");
            let mut paged = merged.pages().expect("paged");
            merger.give_back(&mut paged).expect("given back");
            writer.write(paged).expect("written");
        };
        let mut runs = runs().into_iter();
        let mut first = preparer.start();
        let mut second = preparer.start();
        let mut later = runs.next().expect("a run");
        preparer.feed(&mut second, later.drain(..2).collect()).expect("fed");
        preparer.feed(&mut first, runs.next().expect("a run")).expect("fed");
        write(&mut writer, first);
        let note = |building: &Building| {
            matches!(building.columns[2].lock().expect("unpoisoned").body, Body::Pages(..))
        };
        assert!(!note(&second), "still coded until it is fed again");
        preparer.feed(&mut second, later).expect("fed");
        assert!(note(&second), "turned to pages once fed after the drop");
        write(&mut writer, second);
        let mut last = preparer.start();
        preparer.feed(&mut last, runs.next().expect("a run")).expect("fed");
        write(&mut writer, last);
        writer.finish().expect("commit");
        check(&path);
        fs::remove_file(path).expect("remove");
    }

    /// A stripe is held to [`STRIPE_PARTS`] across all its batches, not only within one.
    #[test]
    fn a_stripe_fed_more_parts_than_it_holds_is_refused() {
        let path = path("overfed");
        let writer = Writer::create(&path, "t", fields()).expect("a file");
        let preparer = writer.preparer();
        let mut building = preparer.start();
        preparer.feed(&mut building, stripe(0, STRIPE_PARTS - 1)).expect("fed");
        assert_eq!(building.parts(), STRIPE_PARTS - 1);
        assert!(preparer.feed(&mut building, stripe(STRIPE_PARTS, 2)).is_err());
        drop(writer);
        let _ = fs::remove_file(path);
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

    /// A table whose `url` repeats twenty values for its first five parts and never repeats after,
    /// the way a log's URLs look once its first hours are past, next to a `city` that repeats
    /// throughout.
    fn turning(id: usize) -> [Value; 3] {
        let url = match id {
            _ if id % 13 == 0 => Value::Null,
            _ if id < 5 * PART => Value::Varchar(format!("https://example.com/{}", id % 20)),
            _ => Value::Varchar(format!("https://example.com/page/{id}")),
        };
        [Value::BigInt(id as i64), Value::Varchar(format!("city {}", id % 13)), url]
    }

    fn turning_fields() -> Vec<Field> {
        vec![
            Field::required("id", LogicalType::BigInt),
            Field::new("city", LogicalType::Varchar),
            Field::new("url", LogicalType::Varchar),
        ]
    }

    fn turning_stripe(
        rows: fn(usize) -> [Value; 3],
        first: usize,
        parts: usize,
    ) -> Vec<((u64, u64), Chunk)> {
        (first..first + parts)
            .map(|part| {
                let rows = (part * PART..(part + 1) * PART).map(rows).collect::<Vec<_>>();
                let column = |at: usize| {
                    let values = rows.iter().map(|row| row[at].clone()).collect::<Vec<_>>();
                    Vector::from_values(turning_fields()[at].ty.clone(), &values).expect("a column")
                };
                let chunk = Chunk::new(vec![column(0), column(1), column(2)]).expect("a chunk");
                ((part as u64, 0), chunk)
            })
            .collect()
    }

    fn turning_runs() -> Vec<Vec<((u64, u64), Chunk)>> {
        vec![
            turning_stripe(turning, 0, 5),
            turning_stripe(turning, 5, 5),
            turning_stripe(turning, 10, 3),
        ]
    }

    /// Reads every row of the turned table back and checks what the reader says about `url`.
    fn check_turned(path: &PathBuf) {
        let reader = Reader::open(path).expect("reopen");
        assert_eq!(reader.parts(), 13);
        assert_eq!(reader.table().demoted, [false, false, true], "only url is demoted");
        for part in 0..13 {
            let chunk = reader.read(part, &[0, 1, 2]).expect("a part");
            let url = chunk.column(2).expect("url");
            assert!(url.stable_dictionary_parts().is_none(), "part {part} hands out no codes");
            for at in 0..PART {
                let want = turning(part * PART + at);
                for (column, value) in want.iter().enumerate() {
                    assert_eq!(&chunk.value_at(at, column), value, "part {part} row {at}");
                }
            }
        }
        // Everything the dictionary would have vouched for covers only the first stripes.
        assert_eq!(reader.distinct_values(2).expect("asked"), None);
        assert_eq!(reader.text_extremes(2).expect("asked"), None);
        assert_eq!(reader.exact_frequencies(2).expect("asked"), None);
        assert_eq!(reader.top_frequencies(2, 5).expect("asked"), None);
        assert!(!reader.skips_codes(0, 2, &[0]).expect("asked"), "no code proves a value absent");
        // `city` keeps its dictionary and all of it.
        assert_eq!(reader.distinct_values(1).expect("asked"), Some(13));
        assert!(reader.text_extremes(1).expect("asked").is_some());
    }

    /// A column whose second stripe is nearly all new values stops growing its dictionary there,
    /// whether the later stripes were prepared before that decision or after it, and every row
    /// reads back.
    #[test]
    fn a_column_that_turns_unique_is_demoted_and_reads_back() {
        let alone = path("demoted-alone");
        let mut writer = Writer::create(&alone, "t", turning_fields()).expect("a file");
        let preparer = writer.preparer();
        let mut runs = turning_runs().into_iter();
        writer.append_stripe(runs.next().expect("a run")).expect("a stripe");
        assert!(preparer.coded[2].load(Atomic::Relaxed), "url repeats in its first stripe");
        for run in runs {
            writer.append_stripe(run).expect("a stripe");
        }
        assert!(!preparer.coded[2].load(Atomic::Relaxed), "url was demoted");
        assert!(preparer.coded[1].load(Atomic::Relaxed), "city kept its dictionary");
        writer.finish().expect("commit");
        check_turned(&alone);

        let split = path("demoted-split");
        let mut writer = Writer::create(&split, "t", turning_fields()).expect("a file");
        let preparer = writer.preparer();
        let prepared = turning_runs()
            .into_iter()
            .map(|run| preparer.prepare(run).expect("prepared"))
            .collect::<Vec<_>>();
        for one in prepared {
            writer.append_prepared(one).expect("a stripe");
        }
        writer.finish().expect("commit");
        check_turned(&split);

        fs::remove_file(alone).expect("remove");
        fs::remove_file(split).expect("remove");
    }

    /// A table where `url` takes a quarter of its rows as new values every stripe, which keeps its
    /// dictionary under the per-stripe rule, and `city` stops growing after its first stripe.
    fn growing(id: usize) -> [Value; 3] {
        let url = Value::Varchar(format!("https://example.com/{}", id / 4));
        [Value::BigInt(id as i64), Value::Varchar(format!("city {}", id % 13)), url]
    }

    /// Once the dictionaries together pass the cap, the column that grew the most stops and the
    /// one that did not grow keeps its dictionary.
    #[test]
    fn the_dictionary_cap_demotes_the_column_that_grew_most() {
        let path = path("capped");
        let mut writer = Writer::create(&path, "t", turning_fields())
            .expect("a file")
            .with_dictionary_cap(1 << 30);
        let preparer = writer.preparer();
        writer.append_stripe(turning_stripe(growing, 0, 5)).expect("a stripe");
        assert!(preparer.coded[2].load(Atomic::Relaxed), "url is under the cap");
        assert!(preparer.coded[1].load(Atomic::Relaxed), "city is under the cap");

        writer.coded.cap(1);
        writer.append_stripe(turning_stripe(growing, 5, 5)).expect("a stripe");
        assert!(!preparer.coded[2].load(Atomic::Relaxed), "url grew most and was demoted");
        assert!(preparer.coded[1].load(Atomic::Relaxed), "city grew nothing and keeps it");
        writer.append_stripe(turning_stripe(growing, 10, 3)).expect("a stripe");
        assert!(preparer.coded[1].load(Atomic::Relaxed), "city still grows nothing");
        writer.finish().expect("commit");

        let reader = Reader::open(&path).expect("reopen");
        assert_eq!(reader.table().demoted, [false, false, true]);
        for part in 0..13 {
            let chunk = reader.read(part, &[0, 1, 2]).expect("a part");
            for at in 0..PART {
                let want = growing(part * PART + at);
                for (column, value) in want.iter().enumerate() {
                    assert_eq!(&chunk.value_at(at, column), value, "part {part} row {at}");
                }
            }
        }
        assert_eq!(reader.distinct_values(1).expect("asked"), Some(13));
        assert_eq!(reader.distinct_values(2).expect("asked"), None);
        fs::remove_file(path).expect("remove");
    }
}
