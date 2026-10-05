//! A table: a name, some columns, and the rows.

use std::borrow::Cow;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use rudb_common::bounds::{Bound, Frequencies, Zones};
use rudb_common::stat::{Provenance, Stat};
use rudb_common::{
    Clustering, ColumnFacts, DeclaredType, Error, Field, LogicalType, Result, Value,
};
use rudb_encoding::sequence::Sequence;
use rudb_native::{
    Common, FrequencyCodes, FrequencyOccurrences, FrequencyPrefix, PairFrequencyCounts,
    Reader as NativeReader, StoredPart, Stripes,
};
use rudb_storage::{MemoryTable, Probe, Range};
use rudb_vector::{Chunk, Selection, VECTOR_SIZE, Vector, concat};

use crate::catalog::DETACHED;
use crate::gone::Gone;
use crate::held::Held;
use crate::keys::{ForeignKey, Key, Seen};
use crate::name::{QualifiedName, same_name};
use crate::points::{Edge, Point, Points, Reach, Spot, looks_up};

/// Where table revisions are counted from, one counter for the process.
///
/// One counter rather than one per catalog, because a transaction compares a table in the catalog
/// it took a snapshot of with the same table in the committed catalog, and the two catalogs count
/// their own changes apart from that point on. A number drawn here is never drawn again, so two
/// tables with the same revision are the same rows.
static REVISION: AtomicU64 = AtomicU64::new(1);

/// A revision nothing has been given yet.
#[must_use]
pub fn next_revision() -> u64 {
    REVISION.fetch_add(1, Ordering::Relaxed) + 1
}

/// The last revision handed out, which a snapshot keeps to tell the changes made after it from the
/// ones made before.
#[must_use]
pub fn revision_now() -> u64 {
    REVISION.load(Ordering::Relaxed)
}

/// Refuses a column list that names the same column twice.
///
/// Exported because the binder makes the same check before anything is created. `CREATE OR REPLACE
/// TABLE` drops the old table on its way to creating the new one, so a check that only happened
/// inside [`Table::new`] would report the duplicate after the old table was already gone.
///
/// # Errors
///
/// If two of the columns have the same name, compared the way SQL compares names, which is without
/// regard to case.
pub fn duplicate_check(columns: &[Field]) -> Result<()> {
    for (at, column) in columns.iter().enumerate() {
        if columns[..at].iter().any(|held| same_name(&held.name, &column.name)) {
            // The one that arrived second is the one named, spelled the way it was written rather
            // than the way the first one was. `CREATE TABLE t (Abc INTEGER, aBC VARCHAR)` says aBC.
            return Err(Error::catalog(format!(
                "Column with name {} already exists!",
                column.name
            )));
        }
    }
    Ok(())
}

/// The share of a file's rows an update can have written over before a checkpoint writes the table
/// again rather than the rows beside it: a quarter. Each read of a patched part lays the new rows
/// over the old, and past that point a table written again reads faster than the work saved.
const PATCHED_SHARE: usize = 4;

/// Rows held while a table is being built or read from a committed native snapshot.
#[derive(Debug, Clone)]
pub enum Rows {
    /// Mutable chunks owned by this process.
    Memory(MemoryTable),
    /// Immutable stripes read by projected column from one file.
    Native(NativeReader),
    /// A committed file with rows appended since, which the next checkpoint folds back into one.
    ///
    /// A table stops being one of the two other kinds the moment somebody inserts into it after it
    /// has been written down, which is every table on the second run of a session. The file is
    /// still the file and the rows that arrived since are a table in memory, so the parts of the
    /// two are laid end to end, the file's first, and everything that reads a part by number reads
    /// whichever of them that number lands in.
    ///
    /// What it gives up is the statistics. A file answers most of them exactly out of what its
    /// writer stored and the rows in memory are not in there, so the ones that cannot be combined
    /// without reading the column say nothing at all rather than saying the file's answer as if it
    /// were the table's. The two that add up, which are the null count and the integer sum, are
    /// added up. [`Rows::is_native`] is false here, so a checkpoint does not carry the table
    /// forward as it is. It keeps the file's stripes and writes the rows since after them, which
    /// [`Rows::grown_from`] says where to start.
    Grown(NativeReader, MemoryTable),
    /// A committed file some of whose rows a `DELETE` took out since, or an `UPDATE` wrote over.
    ///
    /// The file stays the file and [`Gone`] says which of its rows are no longer there, so a
    /// delete costs one bit a row it took rather than reading every row it kept into memory. A part
    /// is read and then has its gone rows dropped, which is all a scan sees. The parts keep the
    /// file's numbering, so a part can come back shorter than the file says, down to no rows.
    ///
    /// Like a grown table it gives up the statistics that would count the gone rows, and keeps the
    /// bounds, because a bound over every row of a part is still a bound over the rows left of it.
    ///
    /// The rows an update wrote are kept in [`Gone`] too, each at the place in its part of the row
    /// it replaces, and a read lays them over the part. The bounds of a column an update wrote no
    /// longer hold for the parts it wrote into, and nothing is said about those, while the bounds
    /// of every other column still do.
    ///
    /// Rows appended since go into the table in memory at the end, the way they do for a grown
    /// table, and are numbered after the file's parts. An insert after a delete or an update then
    /// costs the rows it adds rather than a read of the whole table into memory. While the tail is
    /// empty this is the file and its gone rows and nothing else, and says what it always said.
    /// Once it holds rows, the statistics go quiet the way a grown table's do, and a checkpoint
    /// extends the file with them and marks the gone rows again, see [`Rows::grown_from`].
    Masked(NativeReader, Arc<Gone>, MemoryTable),
}

/// Two columns fetched at sparse native row ordinals without materializing their string values.
#[derive(Debug)]
pub struct StablePairCodes {
    /// Signed values from the first column, with nulls in place.
    pub first: Vec<Option<i128>>,
    /// Stable dictionary codes from the second column, with nulls in place.
    pub second: Vec<Option<u32>>,
    /// The one table-wide dictionary those codes name.
    pub dictionary: Arc<Vector>,
}

type StableCodes = (Vec<Option<u32>>, Arc<Vector>);

/// The rows of a dictionary column that hold one of a set of codes, found by
/// [`Rows::coded_rows`].
#[derive(Debug, Default)]
pub struct CodedRows {
    /// The code each of those rows holds.
    pub codes: Vec<u32>,
    /// The signed value of the other column at each row.
    pub others: Vec<Option<i128>>,
    /// The one table-wide dictionary the codes name.
    pub dictionary: Option<Arc<Vector>>,
}

impl Rows {
    /// The rows to append to, turning a committed file into one that has rows in memory beside it.
    ///
    /// # Errors
    ///
    /// Never in practice. The branch that would report one is the committed file that was replaced
    /// on the line above, which the compiler cannot see is gone.
    pub fn to_append(&mut self) -> Result<&mut MemoryTable> {
        // Rows appended after a delete or an update go after the file and its gone rows, in the
        // table in memory a masked file keeps for them.
        if let Self::Native(reader) = self {
            *self = Self::Grown(reader.clone(), Self::tail_of(reader));
        }
        match self {
            Self::Memory(rows) | Self::Grown(_, rows) | Self::Masked(_, _, rows) => Ok(rows),
            Self::Native(_) => Err(Error::internal("a committed table took no append buffer")),
        }
    }

    /// A file with `gone` rows gone and nothing appended yet.
    #[must_use]
    pub fn masked(reader: NativeReader, gone: Arc<Gone>) -> Self {
        let tail = Self::tail_of(&reader);
        Self::Masked(reader, gone, tail)
    }

    /// An empty table in memory with the columns of the file `reader` reads.
    fn tail_of(reader: &NativeReader) -> MemoryTable {
        MemoryTable::new(reader.table().fields().iter().map(|field| field.ty.clone()).collect())
    }

    /// Part `at` of a file read as `part`, holding `columns`, with the rows an update wrote into
    /// it laid over the rows they replace, and every column an update wrote anywhere without the
    /// file's string codes. `slots` names the rows of the part that were read, by the file's
    /// count, when not all of them were.
    fn patched(
        reader: &NativeReader,
        gone: &Gone,
        at: usize,
        columns: &[usize],
        part: Chunk,
        slots: Option<&[u32]>,
    ) -> Result<Chunk> {
        if !columns.iter().any(|&column| gone.touched(column)) {
            return Ok(part);
        }
        let patch = gone.patch(at);
        let fields = reader.table().fields();
        let rows = part.len();
        let mut laid = Vec::with_capacity(part.width());
        for (read, &column) in part.into_columns().into_iter().zip(columns) {
            laid.push(match patch {
                Some(patch) if gone.touched(column) => {
                    let field = fields
                        .get(column)
                        .ok_or_else(|| Error::internal("a read names a column past the table"))?;
                    match slots {
                        Some(slots) => patch.over_rows(&field.ty, &read, column, slots)?,
                        None => patch.over(&field.ty, &read, column)?,
                    }
                }
                // A part the update left alone still loses the file's string codes, because the
                // parts it wrote into have none and a group that took codes from one part and
                // strings from the next would hold every key twice.
                None if gone.touched(column) => read.loosened(),
                _ => read,
            });
        }
        Chunk::with_rows(laid, rows)
    }

    /// Whether an update wrote one of the columns `probes` test in a part of stripe `stripe`, which
    /// is when what the file says about the stripe stops being true.
    fn stripe_changed(
        reader: &NativeReader,
        gone: &Gone,
        stripe: usize,
        columns: impl Iterator<Item = usize> + Clone,
    ) -> bool {
        gone.is_patched()
            && reader
                .stripe_parts()
                .get(stripe)
                .is_some_and(|parts| parts.clone().any(|part| gone.changes(part, columns.clone())))
    }

    /// The rows of part `at` of a file with some rows gone that are still there, or `None` when
    /// all of them are.
    fn live(reader: &NativeReader, gone: &Gone, at: usize) -> Option<Vec<u32>> {
        gone.live(at, reader.part_rows(at))
    }

    /// The file behind a table whose rows are all in it, for whoever wants a stored section.
    ///
    /// `None` for a table in memory, which has no file, and `None` for a grown one, which has a
    /// file and rows that are not in it. A grown table is the case that matters here: a forward
    /// link covers the rows that were in the file when the checkpoint built it, and a row appended
    /// since has no entry in it at all. Answering with the reader would hand out a link that is
    /// correct about a prefix of the table and silent about the rest, and silence reads as *no
    /// parent*, which is a wrong answer rather than a missing one. Refusing here is what makes the
    /// caller fall back to the shape that reads the column.
    ///
    /// The generation stamp in the file catches the other half of the same problem, which is a
    /// table rewritten since the section was built. Both checks are the section 3.1 rule that a
    /// graph section changes the time and never the answer.
    #[must_use]
    pub fn stored(&self) -> Option<&NativeReader> {
        match self {
            Self::Native(reader) => Some(reader),
            Self::Memory(_) | Self::Grown(..) | Self::Masked(..) => None,
        }
    }

    /// How many stripes the committed file contributes, which the row groups are numbered after.
    ///
    /// The parts have the same split and are counted inline, because the reader is already in hand
    /// at every one of those and this one is asked where it is not.
    fn stripes_in_file(&self) -> usize {
        match self {
            Self::Memory(_) => 0,
            Self::Native(reader) | Self::Grown(reader, _) | Self::Masked(reader, ..) => {
                reader.stripe_parts().len()
            }
        }
    }

    /// Exact leading value frequencies, most common first.
    ///
    /// A file proves a count descending prefix out of its stored synopsis. A table in memory has no
    /// prefix to prove: the list the tally built is every value of the column or it is nothing, so
    /// the leading `top` of it are the leading `top` of the column however short the list is, and a
    /// list of three values is the whole answer to a request for five.
    pub fn top_frequencies(&self, column: usize, top: usize) -> Result<Option<Vec<(Value, u64)>>> {
        match self {
            Self::Memory(rows) => rows.frequencies(column),
            Self::Native(reader) => reader.top_frequencies(column, top),
            // A count descending prefix of the file is not one of the table, because a value the
            // rows in memory hold a hundred of can be anywhere in the file's list or absent from it.
            Self::Grown(_, _) | Self::Masked(..) => Ok(None),
        }
    }

    /// Exact leading value frequencies with a bound on every value left out of them.
    ///
    /// The same list [`top_frequencies`] proves a prefix of, handed over with the bound instead of
    /// the proof, so that a caller holding a predicate can do the proving itself. A filter naming
    /// the column being grouped removes whole values from the list and can never split one or merge
    /// two, so the bound on what the list left out survives the filter unchanged and the proof is
    /// the same comparison against a shorter list. A caller without a predicate wants the method
    /// above, which already makes it.
    ///
    /// A table in memory carries a bound of zero or no list at all, because its tally holds every
    /// value of the column or it holds nothing.
    ///
    /// # Errors
    ///
    /// If the column is outside the schema or a stored value does not fit its declared type.
    ///
    /// [`top_frequencies`]: Self::top_frequencies
    pub fn frequency_prefix(&self, column: usize) -> Result<Option<FrequencyPrefix>> {
        match self {
            Self::Memory(rows) => Ok(rows
                .frequencies(column)?
                .map(|entries| FrequencyPrefix { entries, omitted_max: 0 })),
            Self::Native(reader) => reader.frequency_prefix(column),
            // Neither half holds the other's rows, so neither the list nor the bound is the table's.
            Self::Grown(_, _) | Self::Masked(..) => Ok(None),
        }
    }

    /// Query-specific host aggregates are not used, including in older native files.
    pub fn host_groups(
        &self,
        _column: usize,
        _minimum_count: u64,
    ) -> Result<Option<Vec<rudb_native::host::HostEntry>>> {
        Ok(None)
    }

    /// Every value of one column with its exact row count, when something has all of them.
    ///
    /// Only ever an answer for a column with few enough distinct values. A file answers when the
    /// synopsis its writer stored never had to drop a value. A table in memory answers when the tally
    /// it built as the rows arrived stayed under its cap, which is `rudb_storage::TALLY_VALUES`.
    pub fn exact_frequencies(&self, column: usize) -> Result<Option<Vec<(Value, u64)>>> {
        match self {
            Self::Memory(rows) => rows.frequencies(column),
            Self::Native(reader) => reader.exact_frequencies(column),
            // Exact means every value with its row count, and neither half has the other's rows.
            Self::Grown(_, _) | Self::Masked(..) => Ok(None),
        }
    }

    /// How many distinct non-null values one column holds, when the rows are stored somewhere that
    /// already knows it exactly.
    ///
    /// Exactly, because this is read as an answer and not as a guess: `known_rows` in `rudb-exec`
    /// turns it straight into the result of a `COUNT(DISTINCT c)`. A file answers from a dictionary
    /// that holds every distinct value once. A table in memory answers from the sketch it built as
    /// the rows arrived, and only while that sketch is inside the regime where it is holding every
    /// distinct hash there was rather than estimating from the ones it kept.
    ///
    /// `None` means whoever asked has to count the rows the ordinary way. For a memory table that is
    /// a column with at least `rudb_encoding::sketch::DEFAULT_K` distinct values in it, and
    /// [`Rows::distincts`] is where the estimate for one of those comes out.
    pub fn distinct_values(&self, column: usize) -> Result<Option<u64>> {
        match self {
            Self::Memory(rows) => Ok(rows.distinct_values(column)),
            Self::Native(reader) => reader.distinct_values(column),
            // Two exact counts do not add up, because a value in both halves is one value and the
            // sum says two, and this answer is read as the result of a `COUNT(DISTINCT c)`.
            Self::Grown(_, _) | Self::Masked(..) => Ok(None),
        }
    }

    /// How many rows of one column are null, which both kinds of table already know.
    ///
    /// An in memory table counts the validity mask of every chunk as it arrives, so this is exact for
    /// a column in any form. See `MemoryTable::null_count`.
    pub fn null_count(&self, column: usize) -> Result<Option<u64>> {
        match self {
            Self::Memory(rows) => Ok(Some(rows.null_count(column)? as u64)),
            Self::Native(reader) => reader.null_count(column).map(Some),
            // The file counted the gone rows too, and which of them were null is a read away.
            Self::Masked(..) => Ok(None),
            // Nulls do add up, because a null row is a row and both halves counted every one of
            // theirs, so this one stays exact rather than going quiet like the rest.
            Self::Grown(reader, rows) => {
                let held = reader.null_count(column)?;
                Ok(Some(held.saturating_add(rows.null_count(column)? as u64)))
            }
        }
    }

    /// The smallest and the largest value of one string column, when the rows are stored somewhere
    /// that already knows.
    ///
    /// A file reads the two ends of the sorted order beside its dictionary. A table in memory reads
    /// the two ends of the values its tally holds, which is an answer while the column is under
    /// `rudb_storage::TALLY_VALUES` distinct values. Both are the case the zone maps cannot cover:
    /// a string column's ends are allowed to be wider than its rows, so `exact_extremes` refuses
    /// them and what is left is reading every row.
    pub fn text_extremes(&self, column: usize) -> Result<Option<(Value, Value)>> {
        match self {
            Self::Memory(rows) => rows.text_extremes(column),
            Self::Native(reader) => reader.text_extremes(column),
            // Widening one half's pair with the other's would mean ordering two values, and a
            // value here is not ordered, which is what the kernels are for. Both ends of a string
            // column are a read of the column away, so saying nothing costs the caller that.
            Self::Grown(_, _) | Self::Masked(..) => Ok(None),
        }
    }

    /// The smallest and the largest value of one column, when every chunk or stripe of it wrote ends
    /// it had really looked at.
    pub fn exact_extremes(&self, column: usize) -> Result<Option<(Bound, Bound)>> {
        match self {
            Self::Memory(rows) => rows.exact_extremes(column),
            Self::Native(reader) => reader.exact_extremes(column),
            // The same as the pair above, and this one is asked of every column rather than of the
            // string ones, so it is the one worth folding in when a bound learns how to widen.
            Self::Grown(_, _) | Self::Masked(..) => Ok(None),
        }
    }

    /// How many rows of one column are coded into a dictionary and how many rows there are, for
    /// an in memory table. A file and a table grown past one say nothing, since what their parts
    /// are coded as is only known once they are read.
    pub fn dictionary_rows(&self, column: usize) -> Result<Option<(usize, usize)>> {
        match self {
            Self::Memory(rows) => rows.dictionary_rows(column).map(Some),
            Self::Native(_) | Self::Grown(_, _) | Self::Masked(..) => Ok(None),
        }
    }

    /// The sum of one integer column and the rows that went into it, from the zone maps of an in
    /// memory table or the directory of a file.
    pub fn exact_sum(&self, column: usize) -> Result<Option<(i128, u64)>> {
        match self {
            Self::Memory(rows) => rows.exact_sum(column),
            Self::Native(reader) => reader.exact_sum(column),
            // The file's sum less what the rows it records as gone held, while those are all the
            // rows gone. A delete since then has taken rows nobody counted, and rows appended since
            // are rows nobody counted either.
            Self::Masked(_, _, tail) if !tail.is_empty() => Ok(None),
            Self::Masked(reader, gone, _) => {
                let Some(stored) = reader.gone().filter(|stored| stored.total == gone.total())
                else {
                    return Ok(None);
                };
                // An update wrote values the file did not add up.
                if gone.touched(column) {
                    return Ok(None);
                }
                let (Some((held, counted)), Some(Some((lost, fewer)))) =
                    (reader.exact_sum(column)?, stored.sums.get(column).copied())
                else {
                    return Ok(None);
                };
                Ok(held.checked_sub(lost).zip(counted.checked_sub(fewer)))
            }
            // A sum and a row count both add, so the pair adds, and an answer needs both halves
            // because a sum over some of the rows is not a sum over the table.
            Self::Grown(reader, rows) => {
                match (reader.exact_sum(column)?, rows.exact_sum(column)?) {
                    (Some((held, counted)), Some((added, more))) => Ok(held
                        .checked_add(added)
                        .map(|total| (total, counted.saturating_add(more)))),
                    _ => Ok(None),
                }
            }
        }
    }

    /// Sparse numeric frequency candidate rows from a committed native snapshot.
    ///
    /// A file only, and deliberately. What this reports is the row ordinals a bounded candidate set
    /// covers, so a caller can fetch those rows rather than the column, together with a bound on
    /// the values that did not make the set. A table in memory has neither half to give. It keeps
    /// no ordinals per value, and keeping them would mean a row list per value, which grows with
    /// the rows and is the one thing the cap in `rudb_storage::tally` exists to rule out. It also
    /// has no values that did not make the set: its list is every value the column holds or it is
    /// nothing at all, so the bound would always be zero and a caller wanting what this is for
    /// wants [`Rows::exact_frequencies`], which answers it outright.
    pub fn frequency_occurrences(&self, column: usize) -> Result<Option<FrequencyOccurrences>> {
        match self {
            Self::Memory(_) => Ok(None),
            Self::Native(reader) => reader.frequency_occurrences(column),
            // The ordinals a candidate set covers are ordinals of the file, and the table's rows
            // are no longer numbered the way the file numbers them once there are more of them.
            Self::Grown(_, _) | Self::Masked(..) => Ok(None),
        }
    }

    /// One dictionary column's synopsis as the codes its parts carry.
    ///
    /// A file only, for the reason [`Self::frequency_occurrences`] gives: the codes are the file's,
    /// and rows appended since are neither counted in the synopsis nor coded against it.
    pub fn frequency_codes(&self, column: usize) -> Result<Option<FrequencyCodes>> {
        match self {
            Self::Native(reader) => reader.frequency_codes(column),
            Self::Memory(_) | Self::Grown(_, _) | Self::Masked(..) => Ok(None),
        }
    }

    /// The codes of the rows whose dictionary column holds one of `codes`, each with the signed
    /// value of `other` at the same row, and the dictionary the codes index.
    ///
    /// Every part is read, but for its codes and the one other column, which is what a filter on
    /// the column costs without its strings. The parts are split across threads, since a table of
    /// a million rows is a thousand of them. `None` when the table is not a file, a part of the
    /// column is not written against the one dictionary the rest are, or `other` is not signed.
    pub fn coded_rows(
        &self,
        column: usize,
        other: usize,
        codes: &[u32],
    ) -> Result<Option<CodedRows>> {
        let Self::Native(reader) = self else { return Ok(None) };
        let Some(&most) = codes.iter().max() else { return Ok(None) };
        let mut wanted = vec![false; most as usize + 1];
        for &code in codes {
            wanted[code as usize] = true;
        }
        let parts = reader.parts();
        let workers =
            std::thread::available_parallelism().map_or(1, usize::from).min(8).min(parts).max(1);
        let each = parts.div_ceil(workers);
        let columns = [column, other];
        let (wanted, columns) = (&wanted, &columns);
        let found = std::thread::scope(|scope| {
            let handles = (0..workers)
                .map(|worker| {
                    let parts = worker * each..((worker + 1) * each).min(parts);
                    scope.spawn(move || Self::coded_parts(reader, columns, wanted, parts))
                })
                .collect::<Vec<_>>();
            handles
                .into_iter()
                .map(|handle| {
                    handle.join().map_err(|_| Error::internal("a coded row worker panicked"))?
                })
                .collect::<Result<Vec<_>>>()
        })?;
        let mut rows = CodedRows::default();
        let mut dictionary: Option<Arc<Vector>> = None;
        for found in found {
            let Some((held, found)) = found else { return Ok(None) };
            match (&dictionary, held) {
                (Some(dictionary), Some(held)) if !Arc::ptr_eq(dictionary, &held) => {
                    return Ok(None);
                }
                (None, Some(held)) => dictionary = Some(held),
                _ => {}
            }
            rows.codes.extend(found.codes);
            rows.others.extend(found.others);
        }
        Ok(dictionary.map(|dictionary| CodedRows { dictionary: Some(dictionary), ..rows }))
    }

    /// One worker's share of [`Rows::coded_rows`], the parts in `parts` read in order.
    fn coded_parts(
        reader: &NativeReader,
        columns: &[usize],
        wanted: &[bool],
        parts: std::ops::Range<usize>,
    ) -> Result<Option<(Option<Arc<Vector>>, CodedRows)>> {
        let mut rows = CodedRows::default();
        let mut dictionary: Option<Arc<Vector>> = None;
        for part in parts {
            let held = reader.read(part, columns)?;
            let vector = held.column(0)?;
            let Some((part_codes, values)) = vector.stable_dictionary_parts() else {
                return Ok(None);
            };
            match &dictionary {
                Some(held) if !Arc::ptr_eq(held, values) => return Ok(None),
                Some(_) => {}
                None => dictionary = Some(Arc::clone(values)),
            }
            let other = held.column(1)?;
            for (row, &code) in part_codes.iter().enumerate() {
                if !wanted.get(code as usize).copied().unwrap_or(false) || vector.is_null_at(row) {
                    continue;
                }
                rows.codes.push(code);
                if other.is_null_at(row) {
                    rows.others.push(None);
                } else {
                    let Some(value) = other.signed_at(row) else { return Ok(None) };
                    rows.others.push(Some(value));
                }
            }
        }
        Ok(Some((dictionary, rows)))
    }

    /// Query-specific pair leaders are not used, including in older native files.
    pub fn top_pair_frequencies(
        &self,
        _first: usize,
        _second: usize,
        _top: usize,
    ) -> Result<Option<PairFrequencyCounts>> {
        Ok(None)
    }

    /// Reads a signed column and a stable-dictionary column at sorted native row ordinals.
    ///
    /// Unlike [`Self::rows_at`], this may answer more than one vector of positions. It returns raw
    /// values and codes rather than manufacturing an oversized chunk, resolves the part locations
    /// once for the whole request, and lets the two independent columns read in parallel.
    pub fn stable_pair_codes_at(
        &self,
        first: usize,
        second: usize,
        ordinals: &[u64],
    ) -> Result<Option<StablePairCodes>> {
        let Self::Native(reader) = self else { return Ok(None) };
        let (locations, dense) = Self::native_locations(reader, ordinals)?;
        let (first, coded) = std::thread::scope(|scope| {
            let first = scope.spawn(|| Self::read_native_signed(reader, first, &locations, dense));
            let coded = scope.spawn(|| Self::read_native_codes(reader, second, &locations, dense));
            let first = first
                .join()
                .map_err(|_| Error::internal("a signed sparse-fetch worker panicked"))??;
            let coded = coded
                .join()
                .map_err(|_| Error::internal("a code sparse-fetch worker panicked"))??;
            Ok::<_, Error>((first, coded))
        })?;
        let Some((second, dictionary)) = coded else { return Ok(None) };
        Ok(Some(StablePairCodes { first, second, dictionary }))
    }

    /// Reads one stable-dictionary column at sorted native row ordinals without its string values.
    pub fn stable_codes_at(&self, column: usize, ordinals: &[u64]) -> Result<Option<StableCodes>> {
        let Self::Native(reader) = self else { return Ok(None) };
        let (locations, dense) = Self::native_locations(reader, ordinals)?;
        Self::read_native_codes(reader, column, &locations, dense)
    }

    fn native_locations(
        reader: &NativeReader,
        ordinals: &[u64],
    ) -> Result<(Vec<(usize, usize)>, bool)> {
        let mut ends = Vec::with_capacity(reader.parts());
        let mut end = 0_usize;
        for part in 0..reader.parts() {
            end = end.saturating_add(reader.part_rows(part));
            ends.push(end);
        }
        let mut locations = Vec::with_capacity(ordinals.len());
        for &ordinal in ordinals {
            let ordinal = usize::try_from(ordinal)
                .map_err(|_| Error::internal("row ordinal does not fit this platform"))?;
            let part = ends.partition_point(|&end| end <= ordinal);
            if part == ends.len() {
                return Err(Error::internal("row ordinal is past the table"));
            }
            let start = part.checked_sub(1).map_or(0, |before| ends[before]);
            locations.push((part, ordinal - start));
        }
        let distinct = locations
            .iter()
            .enumerate()
            .filter(|&(at, location)| at == 0 || locations[at - 1].0 != location.0)
            .count();
        let dense = distinct.saturating_mul(8) >= reader.parts();
        Ok((locations, dense))
    }

    fn read_native_signed(
        reader: &NativeReader,
        column: usize,
        locations: &[(usize, usize)],
        dense: bool,
    ) -> Result<Vec<Option<i128>>> {
        let mut values = Vec::with_capacity(locations.len());
        let mut from = 0;
        while from < locations.len() {
            let part = locations[from].0;
            let mut upto = from + 1;
            while upto < locations.len() && locations[upto].0 == part {
                upto += 1;
            }
            let held = if dense {
                reader.read(part, &[column])?
            } else {
                reader.read_sparse(part, &[column])?
            };
            let vector = held.column(0)?;
            for &(_, row) in &locations[from..upto] {
                if vector.is_null_at(row) {
                    values.push(None);
                } else {
                    values.push(Some(vector.signed_at(row).ok_or_else(|| {
                        Error::internal("a signed sparse-fetch value has no signed representation")
                    })?));
                }
            }
            from = upto;
        }
        Ok(values)
    }

    fn read_native_codes(
        reader: &NativeReader,
        column: usize,
        locations: &[(usize, usize)],
        dense: bool,
    ) -> Result<Option<StableCodes>> {
        let mut codes = Vec::with_capacity(locations.len());
        let mut dictionary = None;
        let mut from = 0;
        while from < locations.len() {
            let part = locations[from].0;
            let mut upto = from + 1;
            while upto < locations.len() && locations[upto].0 == part {
                upto += 1;
            }
            let held = if dense {
                reader.read(part, &[column])?
            } else {
                reader.read_sparse(part, &[column])?
            };
            let vector = held.column(0)?;
            let Some((part_codes, values)) = vector.stable_dictionary_parts() else {
                return Ok(None);
            };
            if dictionary.as_ref().is_some_and(|held| !Arc::ptr_eq(held, values)) {
                return Ok(None);
            }
            if dictionary.is_none() {
                dictionary = Some(Arc::clone(values));
            }
            for &(_, row) in &locations[from..upto] {
                codes.push((!vector.is_null_at(row)).then(|| part_codes[row]));
            }
            from = upto;
        }
        Ok(dictionary.map(|dictionary| (codes, dictionary)))
    }

    /// Number of rows in one independently readable chunk.
    pub fn chunk_len(&self, at: usize) -> Result<usize> {
        Ok(match self {
            Self::Memory(rows) => rows
                .chunk_len(at)
                .ok_or_else(|| Error::internal("row ordinal names a missing chunk"))?,
            Self::Native(reader) => {
                if at >= reader.parts() {
                    return Err(Error::internal("row ordinal names a missing part"));
                }
                reader.part_rows(at)
            }
            Self::Masked(reader, gone, _) if at < reader.parts() => {
                reader.part_rows(at) - gone.lost(at)
            }
            Self::Grown(reader, _) if at < reader.parts() => reader.part_rows(at),
            Self::Grown(reader, rows) | Self::Masked(reader, _, rows) => rows
                .chunk_len(at - reader.parts())
                .ok_or_else(|| Error::internal("row ordinal names a missing chunk"))?,
        })
    }

    /// Reads selected rows by table-wide ordinal in the order requested.
    pub fn rows_at(
        &self,
        types: &[LogicalType],
        columns: &[usize],
        ordinals: &[u64],
    ) -> Result<Chunk> {
        if columns.len() != types.len() {
            return Err(Error::internal("a row fetch has a different number of columns and types"));
        }
        if let Self::Native(reader) = self {
            return Self::native_rows_at(reader, types, columns, ordinals);
        }
        let mut values = vec![Vec::with_capacity(ordinals.len()); columns.len()];
        let mut cached: Option<(usize, Chunk)> = None;
        for &ordinal in ordinals {
            let ordinal = usize::try_from(ordinal)
                .map_err(|_| Error::internal("row ordinal does not fit this platform"))?;
            let mut start = 0_usize;
            let mut found = None;
            for chunk in 0..self.chunk_count() {
                let len = self.chunk_len(chunk)?;
                if ordinal < start.saturating_add(len) {
                    found = Some((chunk, ordinal - start));
                    break;
                }
                start = start.saturating_add(len);
            }
            let (chunk, row) =
                found.ok_or_else(|| Error::internal("row ordinal is past the table"))?;
            if cached.as_ref().is_none_or(|(held, _)| *held != chunk) {
                cached = Some((chunk, self.read(chunk, columns)?));
            }
            let Some((_, held)) = &cached else {
                return Err(Error::internal("row chunk was not cached"));
            };
            for (at, values) in values.iter_mut().enumerate() {
                values.push(held.value_at(row, at));
            }
        }
        let vectors = values
            .into_iter()
            .zip(types)
            .map(|(values, ty)| Vector::from_values(ty.clone(), &values))
            .collect::<Result<Vec<_>>>()?;
        Chunk::with_rows(vectors, ordinals.len())
    }

    /// Reads a native row fetch across all requested parts and columns in one worker fan-out.
    ///
    /// Ordinary scans already parallelize by part in the pipeline above the reader. A late fetch is
    /// deliberately one pipeline instance and commonly asks for all hundred ClickBench columns from
    /// rows in several parts. Keeping the workers alive across those parts avoids a scoped thread
    /// launch and join for every winning part.
    fn native_rows_at(
        reader: &NativeReader,
        types: &[LogicalType],
        columns: &[usize],
        ordinals: &[u64],
    ) -> Result<Chunk> {
        if columns.is_empty() {
            return Chunk::with_rows(Vec::new(), ordinals.len());
        }
        let mut ends = Vec::with_capacity(reader.parts());
        let mut end = 0_usize;
        for part in 0..reader.parts() {
            end = end.saturating_add(reader.part_rows(part));
            ends.push(end);
        }
        let mut locations = Vec::with_capacity(ordinals.len());
        for &ordinal in ordinals {
            let ordinal = usize::try_from(ordinal)
                .map_err(|_| Error::internal("row ordinal does not fit this platform"))?;
            let part = ends.partition_point(|&end| end <= ordinal);
            if part == ends.len() {
                return Err(Error::internal("row ordinal is past the table"));
            }
            let start = part.checked_sub(1).map_or(0, |before| ends[before]);
            locations.push((part, ordinal - start));
        }
        // A fetch that reaches most of the table is cheaper read a whole stripe page at a time,
        // because the winners in one stripe then cost one read rather than one read a part. A fetch
        // that reaches a handful of rows is not, because a page is sixty four parts wide and it
        // would be reading all of them to use one. An eighth of the parts is where the bytes a page
        // read wastes stop being worth the calls it saves.
        let mut distinct = 0_usize;
        for (at, location) in locations.iter().enumerate() {
            if at == 0 || locations[at - 1].0 != location.0 {
                distinct += 1;
            }
        }
        let dense = distinct.saturating_mul(8) >= reader.parts();
        const MIN_COLUMNS_PER_WORKER: usize = 16;
        const MAX_WORKERS: usize = 8;
        // A wide fetch amortizes a worker over its columns. A narrow fetch over a full vector of
        // scattered rows amortizes it over the parts each column has to read instead, and keeping
        // two such columns on one worker made a certified pair lookup twice as serial as its data.
        let workers = if locations.len() >= VECTOR_SIZE / 2 {
            columns.len().min(MAX_WORKERS)
        } else {
            columns.len().div_ceil(MIN_COLUMNS_PER_WORKER).min(MAX_WORKERS)
        };
        if workers <= 1 {
            let vectors = Self::read_native_columns(reader, columns, types, &locations, dense)?;
            return Chunk::with_rows(vectors, ordinals.len());
        }
        let width = columns.len().div_ceil(workers);
        let locations = &locations;
        let pieces = std::thread::scope(|scope| {
            let handles = columns
                .chunks(width)
                .zip(types.chunks(width))
                .map(|(columns, types)| {
                    scope.spawn(move || {
                        Self::read_native_columns(reader, columns, types, locations, dense)
                    })
                })
                .collect::<Vec<_>>();
            handles
                .into_iter()
                .map(|handle| {
                    handle
                        .join()
                        .map_err(|_| Error::internal("a native row fetch worker panicked"))?
                })
                .collect::<Result<Vec<_>>>()
        })?;
        let mut vectors = Vec::with_capacity(columns.len());
        for piece in pieces {
            vectors.extend(piece);
        }
        Chunk::with_rows(vectors, ordinals.len())
    }

    fn read_native_columns(
        reader: &NativeReader,
        columns: &[usize],
        types: &[LogicalType],
        locations: &[(usize, usize)],
        dense: bool,
    ) -> Result<Vec<Vector>> {
        let mut pieces = (0..columns.len()).map(|_| Vec::new()).collect::<Vec<Vec<Vector>>>();
        let mut from = 0;
        while from < locations.len() {
            let part = locations[from].0;
            let mut upto = from + 1;
            while upto < locations.len() && locations[upto].0 == part {
                upto += 1;
            }
            let selected = locations[from..upto]
                .iter()
                .map(|&(_, row)| {
                    u32::try_from(row)
                        .map_err(|_| Error::internal("a row within a part exceeds u32"))
                })
                .collect::<Result<Vec<_>>>()?;
            // Rows asked for in order are read at those rows alone, which for a compressed string
            // page is decompressing them and not the rest of the part.
            if selected.windows(2).all(|pair| pair[0] < pair[1]) {
                let held = reader.read_rows(part, columns, &selected, dense)?;
                for (at, pieces) in pieces.iter_mut().enumerate() {
                    pieces.push(held.column(at)?.clone());
                }
                from = upto;
                continue;
            }
            let held = if dense {
                reader.read(part, columns)?
            } else {
                reader.read_sparse(part, columns)?
            };
            for (at, pieces) in pieces.iter_mut().enumerate() {
                pieces.push(held.column(at)?.gather(&selected)?);
            }
            from = upto;
        }
        pieces
            .into_iter()
            .zip(types)
            .map(|(pieces, ty)| {
                if let Some(vector) = concat(ty, &pieces)? {
                    return Ok(vector);
                }
                let values = pieces
                    .iter()
                    .flat_map(|piece| (0..piece.len()).map(|row| piece.value_at(row)))
                    .collect::<Vec<_>>();
                Vector::from_values(ty.clone(), &values)
            })
            .collect()
    }

    /// Column types.
    #[must_use]
    pub fn types(&self) -> Vec<LogicalType> {
        match self {
            Self::Memory(rows) => rows.types().to_vec(),
            Self::Native(reader) | Self::Grown(reader, _) | Self::Masked(reader, ..) => {
                reader.table().fields().iter().map(|field| field.ty.clone()).collect()
            }
        }
    }

    /// Total row count.
    #[must_use]
    pub fn len(&self) -> usize {
        match self {
            Self::Memory(rows) => rows.len(),
            Self::Native(reader) => reader.table().rows(),
            Self::Grown(reader, rows) => reader.table().rows().saturating_add(rows.len()),
            Self::Masked(reader, gone, rows) => {
                reader.table().rows().saturating_sub(gone.total()).saturating_add(rows.len())
            }
        }
    }

    /// Whether there are no rows.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Whether these rows are all already in a committed native snapshot.
    ///
    /// False for a table that has rows in memory beside the file, which is what makes a checkpoint
    /// write that table again rather than carry the file's generation forward and lose them.
    #[must_use]
    pub fn is_native(&self) -> bool {
        matches!(self, Self::Native(_))
    }

    /// The rows of a committed file, with the rows the file records as gone taken out.
    ///
    /// # Errors
    ///
    /// If what the file records names a part it does not have.
    pub fn of_file(reader: NativeReader) -> Result<Self> {
        let Some(stored) = reader.gone() else { return Ok(Self::Native(reader)) };
        let gone = Gone::stored(reader.parts(), stored)?;
        Ok(Self::masked(reader, Arc::new(gone)))
    }

    /// Whether the file behind these rows already says everything about them, so a checkpoint
    /// carries the table forward as it is. A table some rows were deleted from counts once the file
    /// records those rows as gone.
    #[must_use]
    pub fn is_stored(&self) -> bool {
        match self {
            Self::Native(_) => true,
            // The rows gone only ever grow from what the file recorded, so the same count is the
            // same rows. Rows appended since are not in the file at all.
            Self::Masked(reader, gone, rows) => {
                rows.is_empty()
                    && !gone.is_fresh()
                    && reader.gone().map_or(0, |stored| stored.total) == gone.total()
            }
            Self::Memory(_) | Self::Grown(..) => false,
        }
    }

    /// Whether [`Self::marks`] has a record to hand back, without reading anything for it.
    #[must_use]
    pub fn markable(&self) -> bool {
        matches!(self, Self::Masked(reader, gone, rows)
            if rows.is_empty() && !self.is_stored() && Self::few_gone(reader, gone))
    }

    /// Whether few enough of the file's rows are gone or written over for a checkpoint to write
    /// down which, rather than write the rest of the table again. See [`Self::marks`].
    fn few_gone(reader: &NativeReader, gone: &Gone) -> bool {
        gone.total() * 2 <= reader.table().rows()
            && gone.patched_rows() * PATCHED_SHARE <= reader.table().rows()
    }

    /// Whether these are a file's rows with some gone and rows appended since, which a checkpoint
    /// writes by extending the file and marking the gone rows again.
    fn extends_marked(&self) -> bool {
        matches!(self, Self::Masked(reader, gone, rows)
            if !rows.is_empty() && Self::few_gone(reader, gone))
    }

    /// For a table whose file is still the one it was read from and some of whose rows a delete
    /// took out since the file last said, every row gone from it, for a checkpoint to write down
    /// beside the file rather than write the rest of the table again.
    ///
    /// The rows an update wrote over go with it, see `PATCHED_SHARE`.
    ///
    /// A table with rows appended since has a record too, for the checkpoint that extends it, see
    /// [`Self::grown_from`].
    ///
    /// `None` once half the file's rows are gone. The gone rows still take their space in the
    /// file, and past that point writing the rest of the table again is what gives it back, at a
    /// cost no more than the rows that are left.
    ///
    /// The record carries what the gone rows held of each integer column the file keeps a sum
    /// of, so a sum over what is left is still answered without reading the table.
    ///
    /// # Errors
    ///
    /// If a part the new rows went from does not read back.
    pub fn marks(&self) -> Result<Option<rudb_native::GoneRows>> {
        match self {
            Self::Masked(reader, gone, _) if self.markable() || self.extends_marked() => {
                let mut marks = gone.marks();
                marks.sums = reader.gone_sums(&marks, reader.gone())?;
                Ok(Some(marks))
            }
            _ => Ok(None),
        }
    }

    /// For rows appended to a committed native snapshot, how many rows and parts the snapshot
    /// holds. The parts after those are the appended rows, which is all a checkpoint that keeps
    /// the snapshot's stripes has to write.
    ///
    /// The same for a file some rows are gone from, while [`Self::marks`] says which, so the
    /// checkpoint writes the rows appended and the record and not the rest of the table.
    #[must_use]
    pub fn grown_from(&self) -> Option<(usize, usize)> {
        match self {
            Self::Grown(reader, _) => Some((reader.table().rows(), reader.parts())),
            Self::Masked(reader, ..) if self.extends_marked() => {
                Some((reader.table().rows(), reader.parts()))
            }
            Self::Memory(_) | Self::Native(_) | Self::Masked(..) => None,
        }
    }

    /// The rows a `DELETE` took out of the file behind this table, when it has one and some are.
    #[must_use]
    pub fn gone(&self) -> Option<&Gone> {
        match self {
            Self::Masked(_, gone, _) => Some(gone),
            Self::Memory(_) | Self::Native(_) | Self::Grown(..) => None,
        }
    }

    /// Number of independently readable chunks or parts.
    #[must_use]
    pub fn chunk_count(&self) -> usize {
        match self {
            Self::Memory(rows) => rows.chunk_count(),
            Self::Native(reader) => reader.parts(),
            Self::Grown(reader, rows) | Self::Masked(reader, _, rows) => {
                reader.parts().saturating_add(rows.chunk_count())
            }
        }
    }

    /// The parts of each stripe, in the same numbering [`Self::read`] takes.
    ///
    /// A scan uses it to hand a whole stripe to one worker instead of handing its parts to whoever
    /// asks first. An in memory table answers its row groups here, because a group and a stripe are
    /// the same thing to a scan: a run of chunks that were written together and can be ruled out
    /// together. It used to answer nothing, and a scan of it was handed a morsel per chunk.
    #[must_use]
    pub fn stripe_parts(&self) -> Vec<std::ops::Range<usize>> {
        match self {
            Self::Memory(rows) => rows.group_parts(),
            Self::Native(reader) => reader.stripe_parts(),
            // The file's stripes and then the row groups of what arrived since, moved up by the
            // parts in front of them so that a range here still names parts [`Self::read`] takes.
            Self::Grown(reader, rows) | Self::Masked(reader, _, rows) => {
                let parts = reader.parts();
                let mut stripes = reader.stripe_parts();
                stripes.extend(
                    rows.group_parts()
                        .into_iter()
                        .map(|group| group.start + parts..group.end + parts),
                );
                stripes
            }
        }
    }

    /// How many rows one stripe holds, in the numbering [`Self::stripe_parts`] hands back.
    ///
    /// The number a scan divides its work by, and it comes off the directory on both sides rather
    /// than out of a walk over [`Self::chunk_len`]. Both sides were already holding it.
    #[must_use]
    pub fn stripe_rows(&self, stripe: usize) -> usize {
        match self {
            Self::Memory(rows) => rows.group_rows(stripe),
            Self::Native(reader) => reader.stripe_rows(stripe),
            Self::Masked(reader, gone, rows) => {
                let held = self.stripes_in_file();
                if stripe >= held {
                    return rows.group_rows(stripe - held);
                }
                let lost = reader
                    .stripe_parts()
                    .get(stripe)
                    .map_or(0, |parts| parts.clone().map(|part| gone.lost(part)).sum());
                reader.stripe_rows(stripe) - lost
            }
            Self::Grown(reader, rows) => {
                let held = self.stripes_in_file();
                if stripe < held {
                    reader.stripe_rows(stripe)
                } else {
                    rows.group_rows(stripe - held)
                }
            }
        }
    }

    /// Asks a native reader to keep `stripes` stripes of every column it reads.
    ///
    /// Nothing for an in memory table, which holds all of its chunks anyway.
    pub fn keep_stripes(&self, stripes: usize) {
        match self {
            Self::Memory(_) => {}
            Self::Native(reader) | Self::Grown(reader, _) | Self::Masked(reader, ..) => {
                reader.keep_stripes(stripes);
            }
        }
    }

    /// Reads only projected columns.
    pub fn read(&self, at: usize, columns: &[usize]) -> Result<Chunk> {
        match self {
            Self::Memory(rows) => rows.read(at, columns),
            Self::Native(reader) => reader.read(at, columns),
            Self::Masked(reader, _, rows) if at >= reader.parts() => {
                rows.read(at - reader.parts(), columns)
            }
            Self::Masked(reader, gone, rows) => {
                let read = reader.read(at, columns)?;
                let part = Self::patched(reader, gone, at, columns, read, None)?;
                let part = match Self::live(reader, gone, at) {
                    // Gathered rather than selected, so what a checkpoint writes from it is a plain
                    // column and not a window onto the rows it is leaving behind.
                    Some(live) => {
                        let mut columns = Vec::with_capacity(part.width());
                        for column in 0..part.width() {
                            columns.push(part.column(column)?.gather(&live)?);
                        }
                        Chunk::with_rows(columns, live.len())?
                    }
                    None => part,
                };
                // Without the file's codes once rows arrived since, for the reason a grown
                // table's parts are.
                Ok(if rows.is_empty() { part } else { part.loosened() })
            }
            // A part of the file keeps the file's string codes, and they are not the codes of the
            // rows that arrived since, which have none. A caller told the codes were stable would
            // count the file's rows by code and never meet the others.
            Self::Grown(reader, rows) => {
                if at < reader.parts() {
                    reader.read(at, columns).map(Chunk::loosened)
                } else {
                    rows.read(at - reader.parts(), columns)
                }
            }
        }
    }

    /// Reads only selected positions of one part and the requested columns.
    ///
    /// A native reader can avoid decoding a whole string page when an earlier predicate left only
    /// a few positions, and it does not keep the stripe's pages, since a caller reading sparsely
    /// reaches only a few parts of a stripe. The other stores use their ordinary part read and
    /// gather the same rows.
    pub fn read_selected(&self, at: usize, columns: &[usize], positions: &[u32]) -> Result<Chunk> {
        self.read_rows_of(at, columns, positions, false)
    }

    /// Reads the requested columns of one part at the rows `positions` names, which rise.
    ///
    /// For a scan that read some columns first and ran its filters over them, and now wants the
    /// rest only for the rows it kept. Unlike [`Self::read_selected`] a native reader keeps the
    /// stripe's pages, because the scan goes on to read the next part of the same stripe.
    pub fn read_rows(&self, at: usize, columns: &[usize], positions: &[u32]) -> Result<Chunk> {
        self.read_rows_of(at, columns, positions, true)
    }

    fn read_rows_of(
        &self,
        at: usize,
        columns: &[usize],
        positions: &[u32],
        whole: bool,
    ) -> Result<Chunk> {
        match self {
            Self::Native(reader) => reader.read_rows(at, columns, positions, whole),
            // A part of the rows appended since is read the way a table in memory reads one.
            Self::Masked(reader, ..) if at >= reader.parts() => {
                let part = self.read(at, columns)?;
                let mut selected = Vec::with_capacity(part.width());
                for column in 0..part.width() {
                    selected.push(part.column(column)?.gather(positions)?);
                }
                Chunk::with_rows(selected, positions.len())
            }
            // The positions count the rows left, and the file counts every row it wrote.
            // The rows an update wrote are laid over only the rows read.
            Self::Masked(reader, gone, rows) => {
                let held = match Self::live(reader, gone, at) {
                    Some(live) => {
                        let mut held = Vec::with_capacity(positions.len());
                        for &position in positions {
                            held.push(*live.get(position as usize).ok_or_else(|| {
                                Error::internal("a selection keeps a row past its part")
                            })?);
                        }
                        Cow::Owned(held)
                    }
                    None => Cow::Borrowed(positions),
                };
                let part = reader.read_rows(at, columns, &held, whole)?;
                let part = Self::patched(reader, gone, at, columns, part, Some(&held))?;
                Ok(if rows.is_empty() { part } else { part.loosened() })
            }
            Self::Grown(reader, _) if at < reader.parts() => {
                reader.read_rows(at, columns, positions, whole).map(Chunk::loosened)
            }
            Self::Memory(_) | Self::Grown(_, _) => {
                let part = self.read(at, columns)?;
                let mut selected = Vec::with_capacity(part.width());
                for column in 0..part.width() {
                    selected.push(part.column(column)?.gather(positions)?);
                }
                Chunk::with_rows(selected, positions.len())
            }
        }
    }

    /// The rows of one part whose string column `column` holds the pieces of one of `sequences` in
    /// order, or with `negated` the rows whose column holds none of them, nulls in neither, when
    /// the part can say without its strings being read. `None` when it cannot, which is every part
    /// but a compressed text page of a native file.
    ///
    /// # Errors
    ///
    /// As [`Self::read`].
    pub fn rows_holding(
        &self,
        at: usize,
        column: usize,
        sequences: &[Sequence],
        negated: bool,
    ) -> Result<Option<Vec<u32>>> {
        match self {
            Self::Native(reader) => reader.rows_holding(at, column, sequences, negated),
            Self::Grown(reader, _) if at < reader.parts() => {
                reader.rows_holding(at, column, sequences, negated)
            }
            Self::Memory(_) | Self::Grown(_, _) | Self::Masked(..) => Ok(None),
        }
    }

    /// The rows of one part whose string column `column` is one of `literals`, nulls not among
    /// them, when the part can say without its strings being read. `None` when it cannot, as for
    /// [`Self::rows_holding`].
    ///
    /// # Errors
    ///
    /// As [`Self::read`].
    pub fn rows_equal(
        &self,
        at: usize,
        column: usize,
        literals: &[&[u8]],
    ) -> Result<Option<Vec<u32>>> {
        match self {
            Self::Native(reader) => reader.rows_equal(at, column, literals),
            Self::Grown(reader, _) if at < reader.parts() => {
                reader.rows_equal(at, column, literals)
            }
            Self::Memory(_) | Self::Grown(_, _) | Self::Masked(..) => Ok(None),
        }
    }

    /// Whether statistics prove this chunk cannot match.
    #[must_use]
    pub fn skips(&self, at: usize, probes: &[Probe]) -> bool {
        match self {
            Self::Memory(rows) => rows.skips(at, probes),
            Self::Native(reader) => reader.skips(at, probes),
            Self::Masked(reader, _, rows) if at >= reader.parts() => {
                rows.skips(at - reader.parts(), probes)
            }
            Self::Masked(reader, gone, _) => {
                !gone.changes(at, probes.iter().map(|probe| probe.column))
                    && reader.skips(at, probes)
            }
            Self::Grown(reader, rows) => {
                if at < reader.parts() {
                    reader.skips(at, probes)
                } else {
                    rows.skips(at - reader.parts(), probes)
                }
            }
        }
    }

    /// Whether no row of chunk `at` can hold one of the `LIKE` pieces in `needles`, each with the
    /// table column it has to be in. See [`rudb_storage::grams`].
    ///
    /// Only rows in memory keep grams, so a part of a file rules nothing out here.
    #[must_use]
    pub fn lacks(&self, at: usize, needles: &[(usize, Vec<u8>)], workers: usize) -> bool {
        match self {
            Self::Memory(rows) => rows.lacks(at, needles, workers),
            Self::Native(_) => false,
            Self::Grown(reader, rows) | Self::Masked(reader, _, rows) => {
                at >= reader.parts() && rows.lacks(at - reader.parts(), needles, workers)
            }
        }
    }

    /// The stored range of one table column over chunk `at`, or `None` where nothing was stored.
    #[must_use]
    pub fn range_of(&self, at: usize, column: usize) -> Option<Range> {
        let memory = |rows: &MemoryTable, at| rows.zone(at)?.column(column).cloned();
        match self {
            Self::Memory(rows) => memory(rows, at),
            Self::Native(reader) => reader.part_range(at, column),
            Self::Masked(reader, _, rows) if at >= reader.parts() => {
                memory(rows, at - reader.parts())
            }
            Self::Masked(reader, gone, _) => {
                (!gone.changes(at, [column])).then(|| reader.part_range(at, column)).flatten()
            }
            Self::Grown(reader, rows) => {
                if at < reader.parts() {
                    reader.part_range(at, column)
                } else {
                    memory(rows, at - reader.parts())
                }
            }
        }
    }

    /// Whether `rule` rules out chunk `at` from what the stored range of one table column says.
    ///
    /// For a test that is not a [`Probe`], which today is a join's build side keys as a set. A chunk
    /// with no range for the column is read, the same as for [`Self::skips`].
    #[must_use]
    pub fn ruled_by(&self, at: usize, column: usize, rule: &dyn Fn(&Range) -> bool) -> bool {
        let memory = |rows: &MemoryTable, at| {
            rows.zone(at).and_then(|zone| zone.column(column)).is_some_and(rule)
        };
        match self {
            Self::Memory(rows) => memory(rows, at),
            Self::Native(reader) => reader.ruled_by(at, column, rule),
            Self::Masked(reader, _, rows) if at >= reader.parts() => {
                memory(rows, at - reader.parts())
            }
            Self::Masked(reader, gone, _) => {
                !gone.changes(at, [column]) && reader.ruled_by(at, column, rule)
            }
            Self::Grown(reader, rows) => {
                if at < reader.parts() {
                    reader.ruled_by(at, column, rule)
                } else {
                    memory(rows, at - reader.parts())
                }
            }
        }
    }

    /// The same about a whole stripe, from the bounds that are already in memory.
    #[must_use]
    pub fn stripe_ruled_by(
        &self,
        stripe: usize,
        column: usize,
        rule: &dyn Fn(&Range) -> bool,
    ) -> bool {
        let memory = |rows: &MemoryTable, stripe| {
            rows.group_zone(stripe).is_some_and(|zone| zone.column(column).is_some_and(rule))
        };
        match self {
            Self::Memory(rows) => memory(rows, stripe),
            Self::Native(reader) => reader.stripe_ruled_by(stripe, column, rule),
            Self::Masked(_, _, rows) if stripe >= self.stripes_in_file() => {
                memory(rows, stripe - self.stripes_in_file())
            }
            Self::Masked(reader, gone, _) => {
                !Self::stripe_changed(reader, gone, stripe, std::iter::once(column))
                    && reader.stripe_ruled_by(stripe, column, rule)
            }
            Self::Grown(reader, rows) => {
                let held = self.stripes_in_file();
                if stripe < held {
                    reader.stripe_ruled_by(stripe, column, rule)
                } else {
                    memory(rows, stripe - held)
                }
            }
        }
    }

    /// Whether statistics prove every row of this chunk matches.
    ///
    /// The other side of [`Self::skips`], and what a scan holding the filter itself asks before it
    /// runs one. A chunk this answers `true` for is handed up as it was read.
    #[must_use]
    pub fn certain(&self, at: usize, probes: &[Probe]) -> bool {
        match self {
            Self::Memory(rows) => rows.certain(at, probes),
            Self::Native(reader) => reader.certain(at, probes),
            Self::Masked(reader, _, rows) if at >= reader.parts() => {
                rows.certain(at - reader.parts(), probes)
            }
            Self::Masked(reader, gone, _) => {
                !gone.changes(at, probes.iter().map(|probe| probe.column))
                    && reader.certain(at, probes)
            }
            Self::Grown(reader, rows) => {
                if at < reader.parts() {
                    reader.certain(at, probes)
                } else {
                    rows.certain(at - reader.parts(), probes)
                }
            }
        }
    }

    /// Whether the bounds of a whole stripe prove that none of it can match.
    ///
    /// This is the half of [`Self::skips`] that reads nothing, which is what makes it the one to ask
    /// when the question is where the work is rather than whether a part holds any. An in memory
    /// table answers it from the zone of the row group, in the same numbering
    /// [`Self::stripe_parts`] hands back.
    #[must_use]
    pub fn stripe_skips(&self, stripe: usize, probes: &[Probe]) -> bool {
        match self {
            Self::Memory(rows) => rows.group_skips(stripe, probes),
            Self::Native(reader) => reader.stripe_skips(stripe, probes),
            Self::Masked(_, _, rows) if stripe >= self.stripes_in_file() => {
                rows.group_skips(stripe - self.stripes_in_file(), probes)
            }
            Self::Masked(reader, gone, _) => {
                !Self::stripe_changed(reader, gone, stripe, probes.iter().map(|probe| probe.column))
                    && reader.stripe_skips(stripe, probes)
            }
            Self::Grown(reader, rows) => {
                let held = self.stripes_in_file();
                if stripe < held {
                    reader.stripe_skips(stripe, probes)
                } else {
                    rows.group_skips(stripe - held, probes)
                }
            }
        }
    }

    /// The bounds this store keeps per part of itself, for the planner to ask.
    ///
    /// `None` for a table in memory. It has a zone map per chunk and they are as good as the
    /// stripe bounds a file holds, but a chunk is owned by the table rather than shared behind a
    /// reference count, so handing them to a plan means copying them once per statement bound.
    /// The file side has the query that needs this and is where it starts.
    #[must_use]
    pub fn zones(&self) -> Option<Arc<dyn Zones>> {
        match self {
            Self::Memory(_) => None,
            Self::Native(reader) => Some(Arc::new(Stripes::new(reader.clone()))),
            // The file's stripes are not the table's stripes any more, and a bound that covers some
            // of the rows is not a bound, so the planner is told nothing rather than told half.
            Self::Grown(_, _) | Self::Masked(..) => None,
        }
    }

    /// How many rows hold each value, for the planner to ask about one of them.
    ///
    /// The file half only. A table in memory has the lists too, but reaching them needs the column
    /// names and those are in the catalog entry rather than here, so [`Table::frequencies`] is what
    /// the binder asks and this is half of what it answers with.
    #[must_use]
    pub fn frequencies(&self) -> Option<Arc<dyn Frequencies>> {
        match self {
            Self::Memory(_) => None,
            Self::Native(reader) => Some(Arc::new(Common::new(reader.clone()))),
            Self::Grown(_, _) | Self::Masked(..) => None,
        }
    }

    /// How many distinct values each column holds, for the columns the file can say.
    ///
    /// Only the file, because this is the half that reads its own column names out of the stored
    /// schema. A table in memory does not have its names here, so [`Table::distincts`] is where the
    /// two are put together and it is what the binder asks.
    #[must_use]
    pub fn distincts(&self) -> Vec<(String, Stat<u64>)> {
        match self {
            Self::Memory(_) | Self::Grown(_, _) | Self::Masked(..) => Vec::new(),
            // A reader that cannot answer its own directory is a reader that will fail the scan a
            // moment later with the same error, and the planner is not the place to raise it. An
            // empty list reads back as a table nobody counted, which is where this started.
            Self::Native(reader) => rudb_native::distincts(reader).unwrap_or_default(),
        }
    }

    /// What the file knows about its columns, gathered once for as long as it is open.
    ///
    /// `None` for a table in memory or one with rows grown past its file, which answer through
    /// [`Rows::distincts`] and the others instead because what they hold changes.
    #[must_use]
    pub fn facts(&self) -> Option<Arc<ColumnFacts>> {
        match self {
            Self::Memory(_) | Self::Grown(_, _) | Self::Masked(..) => None,
            Self::Native(reader) => Some(rudb_native::facts(reader)),
        }
    }

    /// The columns whose values never go down in row order and hold no null, by name.
    ///
    /// Only a file can say, because only a file keeps a summary of each column's order. A table in
    /// memory, or a file with rows grown past it, answers nothing, which reads back as a table whose
    /// order nobody knows.
    #[must_use]
    pub fn ascending(&self) -> Vec<String> {
        match self {
            Self::Memory(_) | Self::Grown(_, _) => Vec::new(),
            Self::Masked(_, _, rows) if !rows.is_empty() => Vec::new(),
            Self::Native(reader) => rudb_native::ascending(reader),
            // Rows that never went down still do not once some of them are gone, and a column an
            // update wrote may well have.
            Self::Masked(reader, gone, _) => {
                let fields = reader.table().fields();
                rudb_native::ascending(reader)
                    .into_iter()
                    .filter(|name| {
                        !fields
                            .iter()
                            .enumerate()
                            .any(|(at, field)| gone.touched(at) && same_name(&field.name, name))
                    })
                    .collect()
            }
        }
    }

    /// How many bytes a value of each string column takes on average, by name.
    ///
    /// Only a file can say, for the reason [`Rows::ascending`] gives. A column nobody can answer for
    /// is left out.
    #[must_use]
    pub fn widths(&self) -> Vec<(String, u64)> {
        match self {
            Self::Memory(_) | Self::Grown(_, _) => Vec::new(),
            Self::Masked(_, _, rows) if !rows.is_empty() => Vec::new(),
            Self::Native(reader) | Self::Masked(reader, ..) => rudb_native::widths(reader),
        }
    }

    /// One whole in-memory chunk, used by checkpointing and tests.
    ///
    /// Owned rather than borrowed, because a chunk of an in memory table is a window cut out of its
    /// row group's pages rather than something the table is holding. The cut is a reference count
    /// bump per column and what a cut has to rewrite, so this is not the copy the signature used to
    /// promise it was not.
    #[must_use]
    pub fn chunk(&self, at: usize) -> Option<Chunk> {
        match self {
            Self::Memory(rows) => rows.chunk(at),
            Self::Native(_) => None,
            Self::Grown(reader, rows) | Self::Masked(reader, _, rows) => {
                at.checked_sub(reader.parts()).and_then(|at| rows.chunk(at))
            }
        }
    }
}

/// One table.
///
/// The rows are a [`MemoryTable`] because that is what M0 has. When the storage format arrives the
/// field changes and this type does not, which is the reason the catalog holds the rows behind a
/// handle rather than being the rows.
#[derive(Debug, Clone)]
pub struct Table {
    name: QualifiedName,
    columns: Vec<Field>,
    rows: Rows,
    /// What `duckdb_tables()` reports as `table_oid`, stamped by the catalog when this goes in.
    oid: i64,
    /// The order the rows are meant to be stored in, if anybody declared one.
    ///
    /// Held here and written into the native file at checkpoint, and read back out of the file
    /// when the table is bound from one. A table in memory keeps the declaration and nothing acts
    /// on it yet, which is the whole point: the declaration is what a checkpoint needs in order to
    /// not throw the order away, and the loader that honours it is the next piece.
    clustering: Option<Clustering>,
    /// The primary key and the unique constraints, checked on every write.
    keys: Vec<Key>,
    /// The keys held for each of `keys`, built by the first write that needs them.
    seen: Vec<Option<Seen>>,
    /// The `DEFAULT` of each column as the SQL of its expression, or empty when no column has one.
    defaults: Vec<Option<String>>,
    /// The PostgreSQL type each column was declared with, or empty when no column has one.
    types: Vec<Option<DeclaredType>>,
    /// The SQL of each `CHECK` constraint, in the order written.
    checks: Vec<String>,
    /// The foreign keys this table's rows have to meet, in the order written.
    foreign: Vec<ForeignKey>,
    /// The constraints in the order they were written, as far as that is known.
    order: Vec<crate::Constraint>,
    /// The keys written as constraints of the table, `PRIMARY KEY (a)`, rather than on a column,
    /// by place in `keys`. Only `duckdb_tables().sql` tells the two apart.
    apart: Vec<usize>,
    /// The indexes over it, in the order they were created.
    indexes: Vec<crate::Index>,
    /// The sequences its defaults call `nextval` on, which it depends on the way the pin records it:
    /// a `DROP SEQUENCE` without `CASCADE` is refused while this table is there.
    sequences: Vec<QualifiedName>,
    /// Which version of the rows this is, drawn from [`next_revision`] whenever the catalog hands
    /// the table out to be changed. A clone keeps it, so a transaction can tell whether the
    /// committed table is still the one its snapshot holds.
    revision: u64,
    /// Which numbering of the rows this is. An append or an update in place keeps every row where
    /// it was, and anything else draws a new one, so two tables with the same frame agree on what
    /// row number `n` means for every row both of them have.
    frame: u64,
    /// Which placing of the keys this is. A write that leaves every key in the row it was in, an
    /// update of other columns where the rows are or an append, keeps it, and anything else draws
    /// a new one, which is what tells `points` to look again.
    placed: u64,
    /// Where the row of each key is, built the first time a lookup by key asks.
    points: Points,
}

impl Table {
    /// A table with no rows in it.
    ///
    /// # Errors
    ///
    /// If two columns have the same name, which SQL does not allow and which would make a column
    /// reference ambiguous in a way no error message could explain later. The message is DuckDB's,
    /// which names the column and not the table and is a catalog error rather than a binder one,
    /// because the same sentence comes out of `CREATE TABLE t (a INT, a INT)` and out of a
    /// `CREATE TABLE ... AS` whose column list repeats a name.
    pub fn new(name: QualifiedName, columns: Vec<Field>) -> Result<Self> {
        duplicate_check(&columns)?;
        let types = columns.iter().map(|column| column.ty.clone()).collect();
        Ok(Self {
            name,
            columns,
            rows: Rows::Memory(MemoryTable::new(types)),
            oid: DETACHED,
            clustering: None,
            keys: Vec::new(),
            seen: Vec::new(),
            indexes: Vec::new(),
            defaults: Vec::new(),
            types: Vec::new(),
            checks: Vec::new(),
            foreign: Vec::new(),
            order: Vec::new(),
            apart: Vec::new(),
            sequences: Vec::new(),
            revision: next_revision(),
            frame: next_revision(),
            placed: next_revision(),
            points: Points::default(),
        })
    }

    /// A table whose stripes are read from one committed native file.
    ///
    /// # Errors
    ///
    /// If the reader's stored schema has duplicate column names.
    pub fn native(name: QualifiedName, reader: NativeReader) -> Result<Self> {
        let columns = reader.table().fields().to_vec();
        duplicate_check(&columns)?;
        let clustering = reader.table().clustering().cloned();
        let stored = reader.table().constraints();
        let (keys, foreign) = restored(&name, stored);
        let defaults = stored.defaults.clone();
        let types = stored.types.clone();
        let checks = stored.checks.clone();
        let order = restored_order(stored);
        let apart = restored_apart(stored);
        let indexes = restored_indexes(stored);
        Ok(Self {
            name,
            columns,
            rows: Rows::of_file(reader)?,
            oid: DETACHED,
            clustering,
            // Not read into sets here. A key's set is built from the rows the first time a write
            // asks for it, so opening a file of a hundred million keyed rows reads none of them.
            seen: vec![
                None;
                keys.len()
                    + indexes.iter().filter(|index| index.unique && index.plain).count()
            ],
            keys,
            indexes,
            defaults,
            types,
            checks,
            foreign,
            order,
            apart,
            sequences: Vec::new(),
            revision: next_revision(),
            frame: next_revision(),
            placed: next_revision(),
            points: Points::default(),
        })
    }

    /// The keys, foreign keys, defaults, checks and indexes in the form the file stores them, for a
    /// checkpoint to write.
    ///
    /// # Errors
    ///
    /// If a key names a column past what a file can. A foreign key into another schema is left out,
    /// since a file of one schema has no way to name it, and so is its place in the order.
    pub fn stored_constraints(&self) -> Result<rudb_native::Constraints> {
        let places = |columns: &[usize]| {
            columns
                .iter()
                .map(|&column| u16::try_from(column))
                .collect::<std::result::Result<Vec<u16>, _>>()
                .map_err(|_| Error::internal("a key over a column past what a file can name"))
        };
        let place = |at: usize| {
            u16::try_from(at).map_err(|_| Error::internal("a constraint past what a file can name"))
        };
        let mut stored = rudb_native::Constraints::default();
        for key in &self.keys {
            stored.keys.push((places(&key.columns)?, key.primary));
        }
        // Where each foreign key lands among the stored ones, for the order below.
        let mut kept = Vec::with_capacity(self.foreign.len());
        for foreign in &self.foreign {
            // A file of one schema has no way to name a table in another, and a checkpoint that
            // refused would lose every row to keep one declaration, so this one is left out.
            if !same_name(&foreign.table.schema, &self.name.schema)
                || !same_name(&foreign.table.catalog, &self.name.catalog)
            {
                kept.push(None);
                continue;
            }
            kept.push(Some(stored.foreign.len()));
            stored.foreign.push(rudb_native::StoredForeign {
                columns: places(&foreign.columns)?,
                table: foreign.table.table.clone(),
                referenced: places(&foreign.referenced)?,
            });
        }
        if self.defaults.iter().any(Option::is_some) {
            stored.defaults.clone_from(&self.defaults);
            stored.defaults.resize(self.columns.len(), None);
        }
        if self.types.iter().any(Option::is_some) {
            stored.types.clone_from(&self.types);
            stored.types.resize(self.columns.len(), None);
        }
        stored.checks.clone_from(&self.checks);
        // A key is 0, or 4 when it was written apart from its columns, a check 1, a foreign key 2
        // and a `NOT NULL` 3.
        for constraint in &self.order {
            let (kind, at) = match *constraint {
                crate::Constraint::Key(at) if self.apart.contains(&at) => (4, at),
                crate::Constraint::Key(at) => (0, at),
                crate::Constraint::Check(at) => (1, at),
                crate::Constraint::Foreign(at) => match kept.get(at).copied().flatten() {
                    Some(at) => (2, at),
                    None => continue,
                },
                crate::Constraint::NotNull(at) => (3, at),
            };
            stored.order.push((kind, place(at)?));
        }
        for index in &self.indexes {
            stored.indexes.push(rudb_native::StoredIndex {
                name: index.name.clone(),
                unique: index.unique,
                plain: index.plain,
                columns: places(&index.columns)?,
                expressions: index.expressions.clone(),
                sql: index.sql.clone(),
            });
        }
        Ok(stored)
    }

    /// The number the catalog tables join on, and [`DETACHED`] for a table not in a catalog.
    #[must_use]
    pub fn oid(&self) -> i64 {
        self.oid
    }

    /// Stamps the oid, which only [`crate::Catalog::create_table`] does.
    pub(crate) fn stamp(&mut self, oid: i64) {
        self.oid = oid;
    }

    /// Stamps an oid on each index read back out of a file, drawn from `next`.
    pub(crate) fn stamp_indexes(&mut self, mut next: impl FnMut() -> i64) {
        for index in &mut self.indexes {
            index.oid = next();
        }
    }

    /// Whether the file the rows are in holds what this table is declared with, its keys,
    /// defaults, checks, indexes and which columns refuse nulls. True for a table with no file
    /// behind it, which a checkpoint writes whole anyway.
    ///
    /// # Errors
    ///
    /// If a declaration names a column past what a file can.
    pub fn declared_is_stored(&self) -> Result<bool> {
        match &self.rows {
            Rows::Memory(_) => Ok(true),
            Rows::Native(reader) | Rows::Grown(reader, _) | Rows::Masked(reader, ..) => {
                let stored = reader.table().fields().iter().map(|field| field.not_null);
                Ok(stored.eq(self.columns.iter().map(|column| column.not_null))
                    && reader.table().constraints() == &self.stored_constraints()?)
            }
        }
    }

    /// The three part name.
    #[must_use]
    pub fn name(&self) -> &QualifiedName {
        &self.name
    }

    /// The columns, in order.
    #[must_use]
    pub fn columns(&self) -> &[Field] {
        &self.columns
    }

    /// The column types, in order.
    #[must_use]
    pub fn types(&self) -> Vec<LogicalType> {
        self.columns.iter().map(|column| column.ty.clone()).collect()
    }

    /// Where a column sits, by name, under the identifier rule.
    #[must_use]
    pub fn column_index(&self, name: &str) -> Option<usize> {
        self.columns.iter().position(|column| same_name(&column.name, name))
    }

    /// The rows.
    #[must_use]
    pub fn rows(&self) -> &Rows {
        &self.rows
    }

    /// What every stored part of one column is encoded as, which is what `pragma_storage_info`
    /// reports.
    ///
    /// Empty for a table with no file behind it, and for the memory half of a table that has both.
    /// A chunk that has not been written yet has no encoding to report, because the encoder has not
    /// run on it and will not until a checkpoint, so the honest answer is no rows rather than a row
    /// claiming the chunk is stored plain.
    ///
    /// # Errors
    ///
    /// If the column is outside the schema, or a page the reader has to open is invalid.
    pub fn stored(&self, column: usize) -> Result<Vec<StoredPart>> {
        match &self.rows {
            Rows::Memory(_) => Ok(Vec::new()),
            Rows::Native(reader) | Rows::Grown(reader, _) | Rows::Masked(reader, ..) => {
                reader.stored(column)
            }
        }
    }

    /// How many rows hold each value of each column, named, for the planner to ask.
    ///
    /// Here rather than on [`Rows`] for the reason [`Table::distincts`] is: half of it needs the
    /// column names and a table in memory keeps only its types.
    ///
    /// A file answers out of the synopsis its writer stored, which for a wide column is the leading
    /// values and a bound on the rest. A table in memory answers out of the tally `rudb_storage`
    /// built as the rows arrived, which is every value of a narrow column and nothing for a wide one.
    /// The two are different shapes of the same fact and the estimator reads them through one trait.
    #[must_use]
    pub fn frequencies(&self) -> Option<Arc<dyn Frequencies>> {
        let Rows::Memory(rows) = &self.rows else { return self.rows.frequencies() };
        Held::of(rows, &self.columns).map(|held| Arc::new(held) as Arc<dyn Frequencies>)
    }

    /// The order the rows are meant to be stored in, if one was declared.
    #[must_use]
    pub fn clustering(&self) -> Option<&Clustering> {
        self.clustering.as_ref()
    }

    /// Declares the order the rows are meant to be stored in, or clears the declaration.
    ///
    /// Takes effect at the next checkpoint. Nothing reorders the rows that are already here, and
    /// nothing claims they are in this order: a declaration says what the table is for, and the
    /// engine keeps its own per fragment ranges for what the table actually is.
    ///
    /// # Errors
    ///
    /// If the declaration names a column this table does not have, or names one twice.
    pub fn cluster_by(&mut self, clustering: Option<Clustering>) -> Result<()> {
        self.clustering = match clustering {
            None => None,
            // Rebuilt against this table's columns rather than trusted, since the caller built it
            // from a name list and a stale one would store a column index off the end.
            Some(asked) => {
                Some(Clustering::new(asked.columns().to_vec(), asked.width(), &self.columns)?)
            }
        };
        Ok(())
    }

    /// Whether the declaration this table holds is the one its stored file already records.
    ///
    /// False for a table in memory, which has nothing stored to agree with. What a checkpoint asks
    /// before deciding it has nothing to do: a table whose rows are all already in the file still
    /// needs rewriting if somebody declared an order since it was written, and comparing the table
    /// names alone would miss that and lose the declaration without a word.
    #[must_use]
    pub fn clustering_is_stored(&self) -> bool {
        match &self.rows {
            // A table with rows in memory has rows the file does not, so the file is going to be
            // written again whatever the declaration says, and answering false here says so once.
            Rows::Memory(_) | Rows::Grown(_, _) => false,
            Rows::Masked(_, _, rows) if !rows.is_empty() => false,
            // Rows taken out of an order leave the rest in it, so a table with rows gone keeps the
            // declaration its file was written under. Rows an update wrote need not.
            Rows::Masked(reader, gone, _) if gone.is_patched() => {
                reader.table().clustering().is_none() && self.clustering.is_none()
            }
            Rows::Native(reader) | Rows::Masked(reader, ..) => {
                reader.table().clustering() == self.clustering.as_ref()
            }
        }
    }

    /// How many distinct values each column holds, named, for the columns something can say.
    ///
    /// Here rather than on [`Rows`] because half of it needs the column names and only this type has
    /// them for both kinds of table: a file keeps its own schema and a table in memory keeps only
    /// its types, so the names of a memory table's columns are in the catalog entry and nowhere
    /// else.
    ///
    /// A file answers from a dictionary or, failing that, from the span between the two ends of an
    /// integer column, which is a ceiling and comes back certified. A table in memory answers from
    /// the sketch `count.rs` built as the rows arrived, exact for a column under the sketch's k and
    /// estimated at about one and a half percent above it. A column neither of them can say anything
    /// about is left out, and a column left out is the estimator's `Unknown`.
    /// The columns the rows are stored in ascending order of, which [`Rows::ascending`] answers.
    #[must_use]
    pub fn ascending(&self) -> Vec<String> {
        self.rows.ascending()
    }

    /// How many bytes a value of each string column takes on average, which [`Rows::widths`]
    /// answers.
    #[must_use]
    pub fn widths(&self) -> Vec<(String, u64)> {
        self.rows.widths()
    }

    /// What the file behind this table knows about its columns, which [`Rows::facts`] answers.
    #[must_use]
    pub fn facts(&self) -> Option<Arc<ColumnFacts>> {
        self.rows.facts()
    }

    #[must_use]
    pub fn distincts(&self) -> Vec<(String, Stat<u64>)> {
        let Rows::Memory(rows) = &self.rows else { return self.rows.distincts() };
        self.columns
            .iter()
            .enumerate()
            .filter_map(|(at, column)| {
                let (value, exact) = rows.distinct_estimate(at)?;
                let stat = if exact {
                    Stat::exact(value, Provenance::Sketch)
                } else {
                    Stat::estimated(value, Provenance::Sketch)
                };
                Some((column.name.clone(), stat))
            })
            .collect()
    }

    /// Which version of the rows this is. See the field.
    #[must_use]
    pub fn revision(&self) -> u64 {
        self.revision
    }

    /// Which numbering of the rows this is. See the field.
    #[must_use]
    pub fn frame(&self) -> u64 {
        self.frame
    }

    /// The row whose key over the columns `key` is `values`, with the columns `columns`, found
    /// where the table noted the key rather than by reading the table, `13-the-point-path.md`.
    ///
    /// `None` when that would not answer what the plan does: `key` is not the columns of one of the
    /// table's keys, or a value is not one [`looks_up`] takes for its column as it is.
    ///
    /// # Errors
    ///
    /// If the rows cannot be read.
    pub fn point(
        &self,
        key: &[usize],
        values: &[Value],
        columns: &[usize],
    ) -> Result<Option<Point>> {
        Ok(self.spot(key, values, columns)?.map(|found| match found {
            Some((_, chunk)) => Point::Found(chunk),
            None => Point::Absent,
        }))
    }

    /// The first `limit` rows in the order of the key over the one column `key` whose key is on
    /// the `reach` side of `value`, the highest key first when `descending`, with the columns
    /// `columns`, read where the table noted the keys rather than by reading the table.
    ///
    /// `None` when that would not answer what the plan does: `key` is not one of the table's keys
    /// on its own, or its column and `value` are not both of the integer types or both `VARCHAR`.
    ///
    /// # Errors
    ///
    /// If the rows cannot be read.
    pub fn range(
        &self,
        key: usize,
        (reach, value): (Reach, &Value),
        descending: bool,
        limit: usize,
        columns: &[usize],
    ) -> Result<Option<Vec<Chunk>>> {
        let Some(field) = self.columns.get(key) else { return Ok(None) };
        let int = matches!(field.ty, LogicalType::Integer | LogicalType::BigInt);
        let bound = match value {
            Value::Integer(value) if int => Edge::Int(i64::from(*value)),
            Value::BigInt(value) if int => Edge::Int(*value),
            Value::Varchar(value) if field.ty == LogicalType::Varchar => Edge::Text(value),
            _ => return Ok(None),
        };
        let Some((which, _)) = self.held(&[key]) else { return Ok(None) };
        if columns.iter().any(|&column| column >= self.columns.len()) {
            return Ok(None);
        }
        let reach = (reach, bound);
        self.points
            .range(which, &self.rows, self.placed, key, reach, descending, limit, columns)
            .map(Some)
    }

    /// Which of the keys and plain unique indexes, in the order of [`Self::guards`], is over the
    /// columns `key` in any order, and its columns in its own order.
    fn held(&self, key: &[usize]) -> Option<(usize, &[usize])> {
        // The same columns in any order, which with as many of them as the key has is each once.
        let same = |held: &[usize]| {
            held.len() == key.len() && held.iter().all(|column| key.contains(column))
        };
        let unique = self.indexes.iter().filter(|index| index.unique && index.plain);
        self.keys
            .iter()
            .map(|held| held.columns.as_slice())
            .chain(unique.map(|index| index.columns.as_slice()))
            .enumerate()
            .find(|(_, held)| same(held))
    }

    /// Whether [`Self::point`] can find a row by the columns `key`, given values of their types.
    #[must_use]
    pub fn finds_by(&self, key: &[usize]) -> bool {
        self.held(key).is_some()
    }

    /// Whether [`Self::range`] can read the rows in the order of the one column `key`, given a
    /// bound of its type.
    #[must_use]
    pub fn ranges_by(&self, key: usize) -> bool {
        let ranged = self.columns.get(key).is_some_and(|field| {
            matches!(field.ty, LogicalType::Integer | LogicalType::BigInt | LogicalType::Varchar)
        });
        ranged && self.held(&[key]).is_some_and(|(_, held)| held == [key])
    }

    /// [`Self::point`], with where the row is beside it, for a write to the row there.
    ///
    /// # Errors
    ///
    /// If the rows cannot be read.
    pub fn spot(
        &self,
        key: &[usize],
        values: &[Value],
        columns: &[usize],
    ) -> Result<Option<Option<(Spot, Chunk)>>> {
        let fits = key.len() == values.len()
            && key.iter().zip(values).all(|(&column, value)| {
                self.columns.get(column).is_some_and(|field| looks_up(value, &field.ty))
            });
        let Some((which, held)) = self.held(key) else { return Ok(None) };
        if !fits || columns.iter().any(|&column| column >= self.columns.len()) {
            return Ok(None);
        }
        // In the key's own order, so one key is always looked up by the same columns.
        let ordered: Vec<Value>;
        let values = if held == key {
            values
        } else {
            ordered = held
                .iter()
                .filter_map(|column| key.iter().position(|at| at == column))
                .map(|at| values[at].clone())
                .collect();
            &ordered
        };
        self.points.seek(which, &self.rows, self.placed, held, values, columns).map(Some)
    }

    /// Draws a new revision and a new placing, for a table about to be changed.
    pub(crate) fn touch(&mut self) {
        self.revision = next_revision();
        self.placed = next_revision();
        self.points = Points::default();
    }

    /// Draws a new revision and keeps the placing, for a table about to have columns of a row that
    /// are in no key written where the row is, see [`Self::put_row`].
    pub(crate) fn touch_rows(&mut self) {
        self.revision = next_revision();
    }

    /// Writes `values` over the columns `targets` of the row at `spot`, which [`Self::spot`]
    /// found, `row` being every column of the row as it reads afterwards. Says whether it could,
    /// and when it could not the table is as it was and the caller takes the long way.
    ///
    /// What an `UPDATE` of one row by its key does, `13-the-point-path.md` section 13.4. Every row
    /// keeps its number and every key its row, so the frame and the placing stay. A column in a
    /// key is not written here, because the keys held for the next write would be wrong.
    ///
    /// # Errors
    ///
    /// If a column that refuses nulls would hold one, or the spot is not a row of the table.
    pub fn put_row(
        &mut self,
        spot: Spot,
        targets: &[usize],
        values: &[Value],
        row: &Chunk,
    ) -> Result<bool> {
        if self.guards().iter().any(|key| key.columns.iter().any(|column| targets.contains(column)))
        {
            return Ok(false);
        }
        self.refuse_nulls(row)?;
        match &mut self.rows {
            Rows::Memory(rows) => {
                return rows.put_row(spot.part, spot.place as usize, targets, values);
            }
            // A row appended since the file is written where it is in memory.
            Rows::Grown(reader, rows) | Rows::Masked(reader, _, rows)
                if spot.part >= reader.parts() =>
            {
                let part = spot.part - reader.parts();
                return rows.put_row(part, spot.place as usize, targets, values);
            }
            Rows::Native(_) | Rows::Grown(..) | Rows::Masked(..) => {}
        }
        self.patch_kept(&[spot.number], targets, std::slice::from_ref(row))?;
        Ok(true)
    }

    /// Writes `rows`, every column of the table and a row for each of `numbers`, which rise, over
    /// the rows those numbers name, where they are, and says whether it could. When it could not,
    /// some of the rows may be written already, so the caller works on a copy.
    ///
    /// What a commit does with the rows its transaction updated when somebody else committed rows
    /// of the same table since, rather than read every row of the table to write them again. Every
    /// row keeps its number and every key its row, so the frame and the placing stay, and a row
    /// whose key is not the key already there is refused for the same reason [`Self::put_row`]
    /// does not write a column of a key.
    ///
    /// # Errors
    ///
    /// If a column that refuses nulls would hold one, a number is past the table, or there are not
    /// as many rows as numbers.
    ///
    /// # Panics
    ///
    /// Never: the keys it reads back are the ones it read just before.
    pub fn put_rows(&mut self, numbers: &[u64], rows: &[Chunk]) -> Result<bool> {
        if rows.iter().map(Chunk::len).sum::<usize>() != numbers.len() {
            return Err(Error::internal("rows written over with a row short or over"));
        }
        if numbers.is_empty() {
            return Ok(true);
        }
        for chunk in rows {
            self.refuse_nulls(chunk)?;
        }
        let keyed: Vec<usize> =
            self.guards().iter().flat_map(|key| key.columns.iter().copied()).collect();
        let targets: Vec<usize> =
            (0..self.columns.len()).filter(|column| !keyed.contains(column)).collect();
        let given = || rows.iter().flat_map(|chunk| (0..chunk.len()).map(move |row| (chunk, row)));
        // Where each row is, as a chunk and a place in it, with its key checked against the key
        // there. The key columns of a chunk are read once for all of its rows.
        let mut places = Vec::with_capacity(numbers.len());
        let (mut chunk, mut start) = (0, 0_u64);
        let mut len = self.rows.chunk_len(0)? as u64;
        let mut keys: Option<(usize, Chunk)> = None;
        for (&number, (new, row)) in numbers.iter().zip(given()) {
            while number >= start + len {
                start += len;
                chunk += 1;
                len = self.rows.chunk_len(chunk)? as u64;
            }
            let place = (number - start) as usize;
            if !keyed.is_empty() {
                if keys.as_ref().is_none_or(|(at, _)| *at != chunk) {
                    keys = Some((chunk, self.rows.read(chunk, &keyed)?.settled()?));
                }
                let (_, held) = keys.as_ref().expect("read just above");
                for (at, &column) in keyed.iter().enumerate() {
                    if held.try_value_at(place, at)? != new.try_value_at(row, column)? {
                        return Ok(false);
                    }
                }
            }
            places.push((chunk, place));
        }
        // The rows of a file come first and the rows in memory after them, so the numbers that
        // land in the file are the ones in front. Those are kept beside the file and the others
        // are written where they are.
        let parts = match &self.rows {
            Rows::Memory(_) => 0,
            Rows::Native(reader) | Rows::Grown(reader, _) | Rows::Masked(reader, ..) => {
                reader.parts()
            }
        };
        let filed = places.iter().take_while(|&&(chunk, _)| chunk < parts).count();
        if let Rows::Memory(memory) | Rows::Grown(_, memory) | Rows::Masked(_, _, memory) =
            &mut self.rows
        {
            for (&(chunk, place), (new, row)) in places.iter().zip(given()).skip(filed) {
                let values = targets
                    .iter()
                    .map(|&column| new.try_value_at(row, column))
                    .collect::<Result<Vec<_>>>()?;
                if !memory.put_row(chunk - parts, place, &targets, &values)? {
                    return Ok(false);
                }
            }
        }
        if filed > 0 {
            // The rows are laid end to end and only the first `filed` of them are read.
            self.patch_kept(&numbers[..filed], &targets, rows)?;
        }
        Ok(true)
    }

    /// Replaces an empty mutable table with its committed native snapshot.
    ///
    /// # Errors
    ///
    /// If rows are already present or the stored schema differs from this table.
    pub fn commit_native(&mut self, reader: NativeReader) -> Result<()> {
        if !self.rows.is_empty() {
            return Err(Error::not_implemented(
                "streaming a native insert into a table that already has rows",
            ));
        }
        if reader.table().fields() != self.columns {
            return Err(Error::internal("a committed native snapshot changed its table schema"));
        }
        self.clustering = reader.table().clustering().cloned();
        self.rows = Rows::Native(reader);
        // The keys of an empty table, if a write had built them, which are not this table's now.
        self.seen = vec![None; self.guards().len()];
        Ok(())
    }

    /// Points this table at a snapshot of the rows it already holds.
    ///
    /// What a checkpoint does after it has written the file. The rows are not changing, only where
    /// they are read from, which is why this takes a table that has rows where
    /// [`Table::commit_native`] refuses one. The row count is checked rather than trusted, because
    /// a snapshot that lost rows would be a silent deletion and this is the last place to catch it.
    ///
    /// # Errors
    ///
    /// If the snapshot's schema or row count differs from this table's.
    pub fn rebind_native(&mut self, reader: NativeReader) -> Result<()> {
        if reader.table().fields() != self.columns {
            return Err(Error::internal("a committed native snapshot changed its table schema"));
        }
        let clustering = reader.table().clustering().cloned();
        let rows = Rows::of_file(reader)?;
        if rows.len() != self.rows.len() {
            return Err(Error::internal("a committed native snapshot changed its row count"));
        }
        // The file is the record, so the declaration comes back from it rather than being kept
        // from before. If the checkpoint did not write what this table asked for, this is where
        // that shows up, as the declaration going away rather than as a claim nothing backs.
        self.clustering = clustering;
        self.rows = rows;
        Ok(())
    }

    /// The rows, to add to.
    ///
    /// This is the way past the constraint check, and the two `append` methods here are the way
    /// through it. A caller that already knows what it is holding, such as the loader that built
    /// the chunk out of a file the table was declared from, can take this one.
    ///
    /// A table read out of a committed file grows an append buffer here, and what comes back is
    /// that buffer rather than the whole table. Rows that were already in the file are not in it
    /// and are not meant to be: reading the table is [`Table::rows`], which puts the two together.
    ///
    /// # Panics
    ///
    /// Never. The branch that would is the committed file that has just been given a buffer.
    pub fn rows_mut(&mut self) -> &mut MemoryTable {
        self.rows.to_append().expect("a table that was just given somewhere to append to")
    }

    /// Adds a chunk, refusing a null in a column that said it would not have one.
    ///
    /// # Errors
    ///
    /// If the chunk does not match the table, or if a `NOT NULL` column is handed a null. DuckDB
    /// raises a constraint error there and so does this, with the same shape of message, because a
    /// program that catches one by its text is a program rudb has to not surprise.
    pub fn append(&mut self, chunk: Chunk) -> Result<()> {
        self.refuse_nulls(&chunk)?;
        if !self.guards().is_empty() {
            return self.append_all(vec![chunk], 1);
        }
        self.rows.to_append()?.append(chunk)
    }

    /// Adds every chunk of a finished result, with the statistics taken on up to `workers` threads.
    ///
    /// Every chunk is checked before any of them is kept, so a null in the last chunk leaves the
    /// table as it was rather than holding the rows that came before it.
    ///
    /// # Errors
    ///
    /// The same as [`Self::append`].
    pub fn append_all(&mut self, chunks: Vec<Chunk>, workers: usize) -> Result<()> {
        for chunk in &chunks {
            self.refuse_nulls(chunk)?;
        }
        self.append_checked(chunks, workers, false)
    }

    /// [`Self::append_all`] for the rows a transaction added, going into the committed table when
    /// the commit finds rows committed by others since its snapshot. A key that is already there is
    /// refused in the words the pin fails a commit with.
    ///
    /// # Errors
    ///
    /// The same as [`Self::append`].
    pub fn append_committing(&mut self, chunks: Vec<Chunk>, workers: usize) -> Result<()> {
        for chunk in &chunks {
            self.refuse_nulls(chunk)?;
        }
        self.append_checked(chunks, workers, true)
    }

    fn append_checked(
        &mut self,
        chunks: Vec<Chunk>,
        workers: usize,
        committing: bool,
    ) -> Result<()> {
        let seen = self.appended_keys(&chunks, committing)?;
        let appending = self.points.appending(&chunks)?;
        let before = self.rows.len() as u64;
        // Only the last part can change, a file's rows and its gone rows staying where they are.
        let from = self.rows.chunk_count().saturating_sub(1);
        if let Err(error) = self.rows.to_append().and_then(|rows| rows.append_all(chunks, workers))
        {
            self.points = Points::default();
            return Err(error);
        }
        self.points.appended(appending, &self.rows, before, from);
        self.hold_keys(seen);
        Ok(())
    }

    /// Swaps every row of the table for these, which is how an `UPDATE` or a `DELETE` lands.
    ///
    /// The rows are checked before the old ones are let go, so a refused null leaves the table as
    /// it was. The new rows live in memory whatever the table was before, and a table that had a
    /// file behind it is one the file no longer describes, which the next checkpoint writes again.
    ///
    /// # Errors
    ///
    /// The same as [`Self::append`].
    pub fn replace_all(&mut self, chunks: Vec<Chunk>, workers: usize) -> Result<()> {
        for chunk in &chunks {
            self.refuse_nulls(chunk)?;
        }
        let seen = self
            .guards()
            .iter()
            .map(|key| Seen::of(&chunks, key, &self.columns, true))
            .collect::<Result<Vec<_>>>()?;
        let types = self.columns.iter().map(|field| field.ty.clone()).collect();
        let mut rows = MemoryTable::new(types);
        rows.append_all(chunks, workers)?;
        self.rows = Rows::Memory(rows);
        self.hold_keys(seen);
        self.frame = next_revision();
        Ok(())
    }

    /// [`Self::replace_all`] for rows that are the table's own in the same order, some of them with
    /// new values and perhaps some new ones after them, which is how an `UPDATE` lands. Every row
    /// keeps its number, so the frame does too.
    ///
    /// # Errors
    ///
    /// The same as [`Self::append`], and if there are fewer rows than the table has.
    pub fn update_all(&mut self, chunks: Vec<Chunk>, workers: usize) -> Result<()> {
        let count = chunks.iter().map(Chunk::len).sum::<usize>();
        if count < self.rows.len() {
            return Err(Error::internal("an update in place that lost rows"));
        }
        let frame = self.frame;
        self.replace_all(chunks, workers)?;
        self.frame = frame;
        Ok(())
    }

    /// Whether a `DELETE` can mark rows gone here rather than hand back the rows it keeps.
    ///
    /// A table with a file behind it and no rows beside it can, as long as nothing has to be
    /// checked about the rows that stay. A key is, because [`Self::replace_all`] gathers what the
    /// table holds afterwards for the next write to be checked against, and that is the read of
    /// every row this is here to save.
    #[must_use]
    pub fn takes_rows(&self) -> bool {
        let filed = match &self.rows {
            Rows::Native(_) => true,
            Rows::Masked(_, _, tail) => tail.is_empty(),
            Rows::Memory(_) | Rows::Grown(..) => false,
        };
        filed && self.guards().is_empty()
    }

    /// Takes rows out by their number in the table, which rise, without reading the others.
    ///
    /// The file stays where it is and the rows are marked gone beside it, see [`Rows::Masked`].
    /// Every row after the first one taken moves down, so the frame is new, as it is for
    /// [`Self::replace_all`].
    ///
    /// # Errors
    ///
    /// If the table cannot take rows this way, see [`Self::takes_rows`], or a number is past it.
    pub fn take_rows(&mut self, numbers: &[u64]) -> Result<()> {
        if !self.takes_rows() {
            return Err(Error::internal("rows taken out of a table that keeps them in memory"));
        }
        let (reader, mut gone) = match &self.rows {
            Rows::Native(reader) => (reader.clone(), Gone::none(reader.parts())),
            Rows::Masked(reader, gone, _) => (reader.clone(), Gone::clone(gone)),
            _ => return Err(Error::internal("rows taken out of a table that is not a file")),
        };
        let mut numbers = numbers.iter().copied().peekable();
        let mut start = 0_u64;
        let mut slots = Vec::new();
        for part in 0..reader.parts() {
            let rows = reader.part_rows(part);
            let live = gone.live(part, rows);
            let end = start + (rows - gone.lost(part)) as u64;
            slots.clear();
            while let Some(number) = numbers.next_if(|&number| number < end) {
                let at = number
                    .checked_sub(start)
                    .ok_or_else(|| Error::internal("the rows a delete took out do not rise"))?
                    as usize;
                slots.push(live.as_ref().map_or(at as u32, |live| live[at]));
            }
            if !slots.is_empty() {
                gone.take(part, rows, &slots)?;
            }
            start = end;
        }
        if numbers.next().is_some() {
            return Err(Error::internal("a delete took out a row past the table"));
        }
        // A file with every row gone is an empty table, which memory holds for nothing.
        self.rows = if gone.total() >= reader.table().rows() {
            Rows::Memory(MemoryTable::new(self.types()))
        } else if gone.is_empty() && !gone.is_patched() {
            Rows::Native(reader)
        } else {
            Rows::masked(reader, Arc::new(gone))
        };
        self.frame = next_revision();
        Ok(())
    }

    /// Takes the rows `numbers` names out, which rise, whatever holds the table: marked gone
    /// beside a file that [`Self::takes_rows`], and otherwise every row read and the ones that stay
    /// put back with [`Self::replace_all`]. Every row after the first one taken moves down, so the
    /// frame is new.
    ///
    /// # Errors
    ///
    /// If a number is past the table or the numbers do not rise.
    pub fn remove_rows(&mut self, numbers: &[u64], workers: usize) -> Result<()> {
        if numbers.is_empty() {
            return Ok(());
        }
        if self.takes_rows() {
            return self.take_rows(numbers);
        }
        let all = (0..self.columns.len()).collect::<Vec<_>>();
        let mut kept = Vec::with_capacity(self.rows.chunk_count());
        let mut numbers = numbers.iter().copied().peekable();
        let mut start = 0_u64;
        for at in 0..self.rows.chunk_count() {
            let chunk = self.rows.read(at, &all)?.settled()?;
            let end = start + chunk.len() as u64;
            let mut gone = Vec::new();
            while let Some(number) = numbers.next_if(|&number| number < end) {
                let place = number
                    .checked_sub(start)
                    .ok_or_else(|| Error::internal("the rows a delete took out do not rise"))?;
                gone.push(place as u32);
            }
            start = end;
            if gone.is_empty() {
                kept.push(chunk);
                continue;
            }
            let rest = Selection::from_indices(gone).complement(chunk.len());
            if !rest.is_empty() {
                kept.push(chunk.compact(&rest)?);
            }
        }
        if numbers.next().is_some() {
            return Err(Error::internal("a delete took out a row past the table"));
        }
        self.replace_all(kept, workers)
    }

    /// The rows `numbers` names, which rise, as they are now with `values` in the columns
    /// `targets`, for a table that [`Self::takes_rows`]. `values` holds a column for each of
    /// `targets` and a row for each number, in order, and so does what comes back, a chunk for
    /// each part of the file the rows are in.
    ///
    /// What an `UPDATE` that leaves the rest of the table in the file writes, which reads the rows
    /// it changed and none of the others.
    ///
    /// # Errors
    ///
    /// If the table is not a file, a number is past it, or a part does not read back.
    pub fn patched_rows(
        &self,
        numbers: &[u64],
        targets: &[usize],
        values: &[Chunk],
    ) -> Result<Vec<Chunk>> {
        let places = self.places(numbers)?;
        let all = (0..self.columns.len()).collect::<Vec<_>>();
        let laid = targets
            .iter()
            .enumerate()
            .map(|(at, &column)| {
                let field = self
                    .columns
                    .get(column)
                    .ok_or_else(|| Error::internal("an update names a column past the table"))?;
                lay(&field.ty, values, at, numbers.len())
            })
            .collect::<Result<Vec<_>>>()?;
        let mut out = Vec::with_capacity(places.len());
        for place in &places {
            // Sparse, because an update that changed a row in every part has a few rows of each.
            let mut columns = self
                .rows
                .read_selected(place.part, &all, &place.positions)?
                .loosened()
                .into_columns();
            let picks = place.picks();
            for (new, &column) in laid.iter().zip(targets) {
                columns[column] = new.gather(&picks)?;
            }
            out.push(Chunk::with_rows(columns, picks.len())?);
        }
        Ok(out)
    }

    /// Writes `rows`, every column of the table and one row for each of `numbers`, which rise,
    /// over the rows those numbers name, `targets` being the columns that changed. The file stays
    /// where it is and the rows are kept beside it, see [`Rows::Masked`]. Every row keeps its
    /// number, so the frame does too.
    ///
    /// # Errors
    ///
    /// If the table cannot take rows this way, see [`Self::takes_rows`], a number is past it, or a
    /// column that refuses nulls would hold one.
    pub fn patch_rows(&mut self, numbers: &[u64], targets: &[usize], rows: &[Chunk]) -> Result<()> {
        if !self.takes_rows() {
            return Err(Error::internal("rows written over in a table that keeps them in memory"));
        }
        if numbers.is_empty() {
            return Ok(());
        }
        for chunk in rows {
            self.refuse_nulls(chunk)?;
        }
        self.patch_kept(numbers, targets, rows)
    }

    /// [`Self::patch_rows`] once the rows are checked, for a file table whatever keys it has. The
    /// keys held for the next write stay right only when `targets` is in none of them.
    fn patch_kept(&mut self, numbers: &[u64], targets: &[usize], rows: &[Chunk]) -> Result<()> {
        let places = self.places(numbers)?;
        let (reader, mut gone) = match &self.rows {
            Rows::Native(reader) | Rows::Grown(reader, _) => {
                (reader.clone(), Gone::none(reader.parts()))
            }
            Rows::Masked(reader, gone, _) => (reader.clone(), Gone::clone(gone)),
            Rows::Memory(_) => {
                return Err(Error::internal("rows written over in a table that is not a file"));
            }
        };
        let laid = self
            .columns
            .iter()
            .enumerate()
            .map(|(at, field)| lay(&field.ty, rows, at, numbers.len()))
            .collect::<Result<Vec<_>>>()?;
        for place in places {
            let picks = place.picks();
            let columns =
                laid.iter().map(|column| column.gather(&picks)).collect::<Result<Vec<_>>>()?;
            gone.put(place.part, place.slots, Chunk::with_rows(columns, picks.len())?, targets)?;
        }
        // The rows appended since stay where they are, after the file.
        let empty = Rows::Memory(MemoryTable::new(Vec::new()));
        let tail = match std::mem::replace(&mut self.rows, empty) {
            Rows::Grown(_, tail) | Rows::Masked(_, _, tail) => tail,
            Rows::Native(_) | Rows::Memory(_) => Rows::tail_of(&reader),
        };
        self.rows = Rows::Masked(reader, Arc::new(gone), tail);
        Ok(())
    }

    /// Where each of the rows `numbers` names is in the file behind the table, by part, for the
    /// parts that hold any of them.
    fn places(&self, numbers: &[u64]) -> Result<Vec<Place>> {
        let (reader, gone) = match &self.rows {
            Rows::Native(reader) | Rows::Grown(reader, _) => (reader, None),
            Rows::Masked(reader, gone, _) => (reader, Some(&**gone)),
            Rows::Memory(_) => {
                return Err(Error::internal("rows named in a table that is not a file"));
            }
        };
        let mut places = Vec::new();
        let mut at = 0;
        let mut start = 0_u64;
        for part in 0..reader.parts() {
            if at == numbers.len() {
                break;
            }
            let rows = reader.part_rows(part);
            let end = start + (rows - gone.map_or(0, |gone| gone.lost(part))) as u64;
            let first = at;
            while numbers.get(at).is_some_and(|&number| number < end) {
                at += 1;
            }
            if at > first {
                let live = gone.and_then(|gone| gone.live(part, rows));
                let mut positions = Vec::with_capacity(at - first);
                let mut slots = Vec::with_capacity(at - first);
                for &number in &numbers[first..at] {
                    let position = number
                        .checked_sub(start)
                        .ok_or_else(|| Error::internal("the rows an update names do not rise"))?
                        as u32;
                    positions.push(position);
                    slots.push(live.as_ref().map_or(position, |live| live[position as usize]));
                }
                places.push(Place { part, positions, slots, first });
            }
            start = end;
        }
        if at < numbers.len() {
            return Err(Error::internal("an update names a row past the table"));
        }
        Ok(places)
    }

    /// The primary key and the unique constraints.
    #[must_use]
    pub fn keys(&self) -> &[Key] {
        &self.keys
    }

    /// The indexes over it, in the order they were created.
    #[must_use]
    pub fn indexes(&self) -> &[crate::Index] {
        &self.indexes
    }

    /// Every key a write is checked against: the constraints, then each unique index over plain
    /// columns, which the pin checks the same way and with the same sentence.
    #[must_use]
    pub fn guards(&self) -> Vec<Key> {
        let mut guards = self.keys.clone();
        for index in self.indexes.iter().filter(|index| index.unique && index.plain) {
            guards.push(Key { columns: index.columns.clone(), primary: false });
        }
        guards
    }

    /// Adds an index, refusing a unique one over rows that already repeat a key.
    pub(crate) fn add_index(&mut self, index: crate::Index) -> Result<()> {
        if index.unique && index.plain {
            let key = Key { columns: index.columns.clone(), primary: false };
            if self.stored_keys(&key).is_err() {
                return Err(Error::constraint("Data contains duplicates on indexed column(s)"));
            }
        }
        self.indexes.push(index);
        self.seen = vec![None; self.guards().len()];
        Ok(())
    }

    /// Takes away the index at this place in [`Self::indexes`].
    pub(crate) fn drop_index(&mut self, at: usize) {
        self.indexes.remove(at);
        self.seen = vec![None; self.guards().len()];
    }

    /// The `DEFAULT` of a column as the SQL of its expression, or `None` when it has none, which
    /// is a null.
    #[must_use]
    pub fn default(&self, column: usize) -> Option<&str> {
        self.defaults.get(column).and_then(Option::as_deref)
    }

    /// Declares the columns' defaults, one per column.
    pub fn set_defaults(&mut self, defaults: Vec<Option<String>>) {
        self.defaults = defaults;
    }

    /// The PostgreSQL type a column was declared with, or `None` when its declaration named no
    /// PostgreSQL type or the table was made from a query.
    #[must_use]
    pub fn declared_type(&self, column: usize) -> Option<DeclaredType> {
        self.types.get(column).copied().flatten()
    }

    /// Declares the columns' PostgreSQL types, one per column.
    pub fn set_types(&mut self, types: Vec<Option<DeclaredType>>) {
        self.types = types;
    }

    /// The SQL of each `CHECK` constraint, in the order written.
    #[must_use]
    pub fn checks(&self) -> &[String] {
        &self.checks
    }

    /// The sequences its defaults use.
    #[must_use]
    pub fn sequences(&self) -> &[QualifiedName] {
        &self.sequences
    }

    /// Declares the sequences its defaults use.
    pub fn set_sequences(&mut self, sequences: Vec<QualifiedName>) {
        self.sequences = sequences;
    }

    /// Declares the table's `CHECK` constraints.
    pub fn set_checks(&mut self, checks: Vec<String>) {
        self.checks = checks;
    }

    /// The foreign keys this table's rows have to meet, in the order written.
    #[must_use]
    pub fn foreign(&self) -> &[ForeignKey] {
        &self.foreign
    }

    /// Declares the table's foreign keys.
    pub fn set_foreign(&mut self, foreign: Vec<ForeignKey>) {
        self.foreign = foreign;
    }

    /// Declares the order the table's constraints were written in.
    pub fn set_order(&mut self, order: Vec<crate::Constraint>) {
        self.order = order;
    }

    /// Declares which keys were written as constraints of the table rather than on a column, by
    /// place in [`Table::keys`].
    pub fn set_apart(&mut self, apart: Vec<usize>) {
        self.apart = apart;
    }

    /// Whether the key at `at` was written as a constraint of the table, `PRIMARY KEY (a)`, rather
    /// than on its column.
    #[must_use]
    pub fn written_apart(&self, at: usize) -> bool {
        self.apart.contains(&at)
    }

    /// Every constraint of the table, in the order the pin lists them.
    ///
    /// That is the order they were written in, then the `NOT NULL` of each column that has one
    /// nobody wrote, which is a primary key's or one an `ALTER` added. A constraint the kept order
    /// does not know about, because a change after the table was made added it, goes after the
    /// ones it does know, kind by kind.
    #[must_use]
    pub fn constraints(&self) -> Vec<crate::Constraint> {
        use crate::Constraint;
        let valid = |held: Constraint| match held {
            Constraint::Key(at) => at < self.keys.len(),
            Constraint::Check(at) => at < self.checks.len(),
            Constraint::Foreign(at) => at < self.foreign.len(),
            Constraint::NotNull(at) => self.columns.get(at).is_some_and(|field| field.not_null),
        };
        let rest = (0..self.keys.len()).map(Constraint::Key);
        let rest = rest.chain((0..self.checks.len()).map(Constraint::Check));
        let rest = rest.chain((0..self.foreign.len()).map(Constraint::Foreign));
        let rest = rest.chain((0..self.columns.len()).map(Constraint::NotNull));
        let mut out: Vec<Constraint> = Vec::new();
        for held in self.order.iter().copied().chain(rest) {
            if valid(held) && !out.contains(&held) {
                out.push(held);
            }
        }
        out
    }

    /// Declares the table's keys, which makes the columns of a primary key `NOT NULL` as well.
    ///
    /// # Errors
    ///
    /// If a key names a column the table does not have, or if the rows already held break one.
    pub fn set_keys(&mut self, keys: Vec<Key>) -> Result<()> {
        for key in &keys {
            for &column in &key.columns {
                let Some(field) = self.columns.get_mut(column) else {
                    return Err(Error::internal(format!("a key over column {column}")));
                };
                if key.primary {
                    field.not_null = true;
                }
            }
        }
        self.keys = keys;
        self.seen = vec![None; self.guards().len()];
        let seen = self.appended_keys(&[], false)?;
        self.hold_keys(seen);
        Ok(())
    }

    /// The key sets the table holds once these rows are appended, or the refusal of the first key
    /// they repeat. Builds the set of a key from the rows already held the first time it is asked.
    fn appended_keys(&mut self, chunks: &[Chunk], committing: bool) -> Result<Vec<Seen>> {
        let guards = self.guards();
        if guards.is_empty() {
            return Ok(Vec::new());
        }
        // Taken out rather than cloned, so the set is not copied to add a few keys to it. A
        // refusal puts them back as they were. An append that fails after this leaves them out,
        // and the next write builds them again from the rows.
        let mut sets = Vec::with_capacity(guards.len());
        for (at, key) in guards.iter().enumerate() {
            let held = match self.seen[at].take() {
                Some(held) => held,
                None => self.stored_keys(key)?,
            };
            sets.push(held);
        }
        let mut added = Vec::with_capacity(guards.len());
        for (held, key) in sets.iter().zip(&guards) {
            match held.check(chunks, key, &self.columns, committing) {
                Ok(keys) => added.push(keys),
                Err(error) => {
                    self.hold_keys(sets);
                    return Err(error);
                }
            }
        }
        for (held, keys) in sets.iter_mut().zip(added) {
            held.extend(keys);
        }
        Ok(sets)
    }

    /// Refuses rows a transaction is adding when a key of theirs is one somebody committed since
    /// its snapshot, `self` being the committed table and `before` the table as the snapshot has
    /// it. The pin fails the statement there, ahead of the commit, which would fail too.
    ///
    /// A key the snapshot held as well is left alone: the transaction's own copy refuses it, unless
    /// the transaction deleted it, and then adding it again is the transaction's to do.
    ///
    /// # Errors
    ///
    /// The pin's duplicate key error, or if the rows cannot be read.
    pub fn refuse_keys_since(&mut self, before: &Table, chunks: &[Chunk]) -> Result<()> {
        let guards = self.guards();
        if self.revision == before.revision || guards.is_empty() || guards != before.guards() {
            return Ok(());
        }
        if self.seen.len() != guards.len() {
            self.seen = vec![None; guards.len()];
        }
        for (at, key) in guards.iter().enumerate() {
            let held = match self.seen[at].take() {
                Some(held) => held,
                None => self.stored_keys(key)?,
            };
            let then = || match before.seen.get(at) {
                Some(Some(then)) => Ok(then.clone()),
                _ => before.stored_keys(key),
            };
            let refused = held.refuse_added(then, chunks, key, &self.columns);
            self.seen[at] = Some(held);
            refused?;
        }
        Ok(())
    }

    /// The keys of every row the table holds for `key`, read a part at a time and only from the
    /// key's own columns, so building them holds one part beside the set rather than every column
    /// of every row.
    fn stored_keys(&self, key: &Key) -> Result<Seen> {
        let projected = Key { columns: (0..key.columns.len()).collect(), primary: key.primary };
        let fields = key.columns.iter().map(|&at| self.columns[at].clone()).collect::<Vec<_>>();
        let mut seen = Seen::default();
        for chunk in 0..self.rows.chunk_count() {
            seen.absorb(&self.rows.read(chunk, &key.columns)?, &projected, &fields, true)?;
        }
        Ok(seen)
    }

    fn hold_keys(&mut self, seen: Vec<Seen>) {
        if !seen.is_empty() {
            self.seen = seen.into_iter().map(Some).collect();
        }
    }

    /// Puts these rows where the table's are and hands back the ones it held, which is how a
    /// `RETURNING` list is run over the rows a statement wrote by the same plan that reads the
    /// table. The caller puts the held rows back with [`Self::put_back`].
    ///
    /// # Errors
    ///
    /// If a chunk does not fit the table's columns.
    pub fn stand_in(&mut self, chunks: Vec<Chunk>, workers: usize) -> Result<Rows> {
        let types = self.columns.iter().map(|field| field.ty.clone()).collect();
        let mut rows = MemoryTable::new(types);
        rows.append_all(chunks, workers)?;
        Ok(std::mem::replace(&mut self.rows, Rows::Memory(rows)))
    }

    /// The rows [`Self::stand_in`] handed back, in their place again.
    pub fn put_back(&mut self, rows: Rows) {
        self.rows = rows;
    }

    /// Adds one row, which is [`Self::append_rows`] with one row. The row is only read, because
    /// the table builds its values into the columns of its tail, so the caller keeps it.
    ///
    /// # Errors
    ///
    /// As [`Self::append_rows`].
    pub fn append_row(&mut self, row: &[Value]) -> Result<()> {
        if !self.guards().is_empty() {
            return self.append_rows(&[row.to_vec()]);
        }
        for (at, column) in self.columns.iter().enumerate() {
            if column.not_null && row.get(at).is_some_and(Value::is_null) {
                return Err(self.null_in(&column.name));
            }
        }
        self.rows.to_append()?.append_row(row)
    }

    /// Adds rows of single values, refusing a null in a column that said it would not have one.
    ///
    /// # Errors
    ///
    /// If a row is not as wide as the table, if a value will not convert to its column's type, or
    /// if a `NOT NULL` column is handed a null.
    pub fn append_rows(&mut self, rows: &[Vec<Value>]) -> Result<()> {
        if !self.guards().is_empty() {
            let mut staged = MemoryTable::new(self.types());
            staged.append_rows(rows)?;
            let chunks = (0..staged.chunk_count()).filter_map(|at| staged.chunk(at)).collect();
            return self.append_all(chunks, 1);
        }
        for row in rows {
            for (at, column) in self.columns.iter().enumerate() {
                if column.not_null && row.get(at).is_some_and(Value::is_null) {
                    return Err(self.null_in(&column.name));
                }
            }
        }
        self.rows.to_append()?.append_rows(rows)
    }

    /// Checks a chunk against the `NOT NULL` columns before any of it is kept.
    ///
    /// A table with no such column pays one walk of the column list and touches no data, which is
    /// most tables. A column that does refuse nulls is checked through its validity mask when the
    /// mask is the whole story, which is one word per sixty four rows rather than a read per row.
    /// A dictionary or a constant can hold the null in the body it points at instead, where the
    /// mask cannot see it, so those two are asked value by value.
    fn refuse_nulls(&self, chunk: &Chunk) -> Result<()> {
        for (at, column) in self.columns.iter().enumerate() {
            if !column.not_null {
                continue;
            }
            let vector = chunk.column(at)?;
            // Asked through the validity rather than through a value per row. Building a value for
            // every row of every `NOT NULL` column was most of what loading a file cost, and for a
            // string column it copied each row's bytes only to drop them.
            let found = !vector.never_null() && (0..vector.len()).any(|row| vector.is_null_at(row));
            if found {
                return Err(self.null_in(&column.name));
            }
        }
        Ok(())
    }

    /// Makes one change to the table, and with `rows` swaps every row for these, which is the
    /// table as it reads after the change.
    ///
    /// Called on a copy by [`crate::Catalog::alter`], so a refusal part way leaves the table as it
    /// was. The rows of a table read out of a file are brought into memory for a change to its name
    /// or its columns, since the file says the old ones and the next checkpoint writes it again.
    ///
    /// # Errors
    ///
    /// The pin's refusals: a name that is taken, the last column, a column a key or a foreign key
    /// needs, a null in a column that is now `NOT NULL`, and a key the new rows repeat.
    pub(crate) fn alter(
        &mut self,
        alteration: crate::Alteration,
        rows: Option<Vec<Chunk>>,
        workers: usize,
    ) -> Result<()> {
        use crate::Alteration;
        let taken = |columns: &[Field], name: &str| {
            if columns.iter().any(|held| same_name(&held.name, name)) {
                return Err(Error::catalog(format!("Column with name \"{name}\" already exists!")));
            }
            Ok(())
        };
        self.defaults.resize(self.columns.len(), None);
        self.types.resize(self.columns.len(), None);
        let mut moved = true;
        match alteration {
            Alteration::Rename(to) => self.name.table = to,
            Alteration::RenameColumn { column, to, checks } => {
                taken(&self.columns, &to)?;
                self.columns[column].name = to;
                self.checks = checks;
            }
            Alteration::AddColumn { field, default, sequences, declared } => {
                taken(&self.columns, &field.name)?;
                self.columns.push(field);
                self.defaults.push(default);
                self.types.push(declared);
                self.depend_on(sequences);
            }
            Alteration::DropColumn { column, checks } => {
                if self.columns.len() == 1 {
                    return Err(Error::catalog(
                        "Cannot drop column: table only has one column remaining!",
                    ));
                }
                let name = self.columns[column].name.clone();
                for key in &self.keys {
                    if !key.columns.contains(&column) {
                        continue;
                    }
                    if key.columns.len() == 1 {
                        return Err(Error::catalog(format!(
                            "Cannot drop column \"{name}\" because there is a UNIQUE constraint that \
                             depends on it"
                        )));
                    }
                    let names: Vec<&str> =
                        key.columns.iter().map(|&at| self.columns[at].name.as_str()).collect();
                    return Err(Error::catalog(format!(
                        "Cannot drop column \"{name}\" because it is referenced in unique \
                         constraint UNIQUE({})",
                        names.join(", ")
                    )));
                }
                if self.foreign.iter().any(|foreign| foreign.columns.contains(&column)) {
                    return Err(Error::catalog(format!(
                        "Cannot drop column \"{name}\" because there is a FOREIGN KEY constraint \
                         that depends on it"
                    )));
                }
                let shift = |at: &mut usize| {
                    if *at > column {
                        *at -= 1;
                    }
                };
                for key in &mut self.keys {
                    key.columns.iter_mut().for_each(shift);
                }
                for foreign in &mut self.foreign {
                    foreign.columns.iter_mut().for_each(shift);
                }
                // The kept order points at columns and checks by place, and both can move.
                if checks.len() != self.checks.len() {
                    self.order.retain(|held| !matches!(held, crate::Constraint::Check(_)));
                }
                self.order.retain(|&held| held != crate::Constraint::NotNull(column));
                for held in &mut self.order {
                    if let crate::Constraint::NotNull(at) = held {
                        shift(at);
                    }
                }
                self.columns.remove(column);
                self.defaults.remove(column);
                self.types.remove(column);
                self.checks = checks;
                self.clustering = None;
            }
            Alteration::Default { column, default, sequences } => {
                self.defaults[column] = default;
                self.depend_on(sequences);
                moved = false;
            }
            Alteration::NotNull { column, set: false } => {
                if self.keys.iter().any(|key| key.primary && key.columns.contains(&column)) {
                    return Err(Error::catalog(format!(
                        "column \"{}\" is in a primary key",
                        self.columns[column].name
                    )));
                }
                self.columns[column].not_null = false;
                moved = false;
            }
            Alteration::NotNull { column, set: true } => {
                self.columns[column].not_null = true;
                let all: Vec<usize> = (0..self.columns.len()).collect();
                for at in 0..self.rows.chunk_count() {
                    self.refuse_nulls(&self.rows.read(at, &all)?)?;
                }
                moved = false;
            }
            Alteration::Type { column, ty, declared } => {
                self.columns[column].ty = ty;
                self.types[column] = declared;
                self.clustering = None;
            }
            Alteration::AddKey { columns, primary } => {
                self.add_key(columns, primary)?;
                moved = false;
            }
        }
        let rows = match rows {
            Some(rows) => rows,
            None if moved && !matches!(self.rows, Rows::Memory(_)) => {
                let all: Vec<usize> = (0..self.columns.len()).collect();
                let mut held = Vec::with_capacity(self.rows.chunk_count());
                for at in 0..self.rows.chunk_count() {
                    held.push(self.rows.read(at, &all)?);
                }
                held
            }
            None => return Ok(()),
        };
        self.seen = vec![None; self.guards().len()];
        self.replace_all(rows, workers)
    }

    /// Adds a key to a table that already has rows, refusing it the way the pin does when the rows
    /// already break it. No row moves: a checkpoint writes the key and the `NOT NULL` a primary
    /// key brings into the table's directory, beside the rows it already has.
    fn add_key(&mut self, columns: Vec<usize>, primary: bool) -> Result<()> {
        let names = |columns: &[usize]| {
            columns.iter().map(|&at| self.columns[at].name.as_str()).collect::<Vec<_>>()
        };
        // The pin looks at the rows before the keys the table has, so a second primary key over
        // rows that repeat fails on the rows.
        if primary {
            for chunk in 0..self.rows.chunk_count() {
                let chunk = self.rows.read(chunk, &columns)?;
                for at in 0..columns.len() {
                    let vector = chunk.column(at)?;
                    if !vector.never_null() && (0..vector.len()).any(|row| vector.is_null_at(row)) {
                        return Err(Error::constraint(format!(
                            "NOT NULL constraint failed: \"PRIMARY_{}_{}\"",
                            self.name.table,
                            names(&columns).join("_")
                        )));
                    }
                }
            }
        }
        let key = Key { columns, primary };
        if self.stored_keys(&key).is_err() {
            return Err(Error::constraint("Data contains duplicates on indexed column(s)"));
        }
        if primary {
            if let Some(held) = self.keys.iter().find(|key| key.primary) {
                return Err(Error::catalog(format!(
                    "table \"{}\" can have only one primary key: PRIMARY KEY({})",
                    self.name.table,
                    names(&held.columns).join(", ")
                )));
            }
        } else if self.keys.iter().any(|held| !held.primary && held.columns == key.columns) {
            // The pin names the index behind a key after its columns, so a second one over the
            // same columns is a second index of the same name.
            return Err(Error::catalog(format!(
                "an index with that name already exists for this table: UNIQUE_{}_{}",
                self.name.table,
                names(&key.columns).join("_")
            )));
        }
        // The pin lists a key added later after every constraint the table had, and the `NOT
        // NULL` a primary key brings straight after the key.
        self.order = self.constraints();
        self.order.push(crate::Constraint::Key(self.keys.len()));
        // The pin writes a key added later apart from its columns, the way `PRIMARY KEY (a)` is.
        self.apart.push(self.keys.len());
        if primary {
            for &column in &key.columns {
                if !self.columns[column].not_null {
                    self.columns[column].not_null = true;
                    self.order.push(crate::Constraint::NotNull(column));
                }
            }
        }
        self.keys.push(key);
        self.seen = vec![None; self.guards().len()];
        Ok(())
    }

    /// Adds sequences a default now uses to the ones the table depends on.
    fn depend_on(&mut self, sequences: Vec<QualifiedName>) {
        for name in sequences {
            if !self.sequences.contains(&name) {
                self.sequences.push(name);
            }
        }
    }

    /// The error DuckDB raises when a null reaches a column that refuses them.
    fn null_in(&self, column: &str) -> Error {
        Error::constraint(format!("NOT NULL constraint failed: {}.{}", self.name.table, column))
    }
}

/// The keys and foreign keys a table opened from a file was created with.
///
/// A foreign key's table is named in the file by name alone and is in the same schema as the table
/// holding it, so the rest of its name is this table's.
fn restored(
    name: &QualifiedName,
    stored: &rudb_native::Constraints,
) -> (Vec<Key>, Vec<ForeignKey>) {
    let places = |columns: &[u16]| columns.iter().map(|&column| usize::from(column)).collect();
    let keys = stored
        .keys
        .iter()
        .map(|(columns, primary)| Key { columns: places(columns), primary: *primary })
        .collect();
    let foreign = stored
        .foreign
        .iter()
        .map(|foreign| ForeignKey {
            columns: places(&foreign.columns),
            table: QualifiedName { table: foreign.table.clone(), ..name.clone() },
            referenced: places(&foreign.referenced),
        })
        .collect();
    (keys, foreign)
}

/// The order of a table's constraints out of what [`Table::stored_constraints`] wrote, leaving out
/// a kind this build does not know.
fn restored_order(stored: &rudb_native::Constraints) -> Vec<crate::Constraint> {
    stored
        .order
        .iter()
        .filter_map(|&(kind, at)| {
            let at = usize::from(at);
            match kind {
                0 | 4 => Some(crate::Constraint::Key(at)),
                1 => Some(crate::Constraint::Check(at)),
                2 => Some(crate::Constraint::Foreign(at)),
                3 => Some(crate::Constraint::NotNull(at)),
                _ => None,
            }
        })
        .collect()
}

/// The keys [`Table::stored_constraints`] wrote down as written apart from their columns.
fn restored_apart(stored: &rudb_native::Constraints) -> Vec<usize> {
    stored.order.iter().filter(|&&(kind, _)| kind == 4).map(|&(_, at)| usize::from(at)).collect()
}

/// A table's indexes out of what [`Table::stored_constraints`] wrote, with no oid yet: the catalog
/// stamps one on each as it takes the table, see [`Table::stamp_indexes`].
fn restored_indexes(stored: &rudb_native::Constraints) -> Vec<crate::Index> {
    stored
        .indexes
        .iter()
        .map(|index| crate::Index {
            name: index.name.clone(),
            unique: index.unique,
            columns: index.columns.iter().map(|&column| usize::from(column)).collect(),
            plain: index.plain,
            expressions: index.expressions.clone(),
            sql: index.sql.clone(),
            oid: DETACHED,
        })
        .collect()
}

/// Some of the rows an update names that are in one part of a file, see [`Table::places`].
struct Place {
    /// The part.
    part: usize,
    /// Their positions among the rows of the part that are left, which a read takes.
    positions: Vec<u32>,
    /// Their rows in the part by the file's count, which a patch keeps.
    slots: Vec<u32>,
    /// Which of the numbers named is the first of them.
    first: usize,
}

impl Place {
    /// Which of the rows laid end to end, one for each number named, are these.
    fn picks(&self) -> Vec<u32> {
        (self.first..self.first + self.positions.len()).map(|at| at as u32).collect()
    }
}

/// Column `column` of `chunks` laid end to end as one column of `rows` rows of type `ty`.
fn lay(ty: &LogicalType, chunks: &[Chunk], column: usize, rows: usize) -> Result<Vector> {
    let pieces =
        chunks.iter().map(|chunk| chunk.column(column).cloned()).collect::<Result<Vec<_>>>()?;
    let order = (0..rows).collect::<Vec<_>>();
    rudb_vector::assemble::interleave(ty, &pieces, &order)
}

#[cfg(test)]
mod tests {
    use rudb_vector::Vector;

    use super::*;

    fn hits() -> Table {
        Table::new(
            QualifiedName::new("memory", "main", "hits"),
            vec![
                Field::new("UserID", LogicalType::BigInt),
                Field::new("SearchPhrase", LogicalType::Varchar),
            ],
        )
        .expect("two columns with different names")
    }

    #[test]
    fn a_column_is_found_however_it_is_spelled() {
        let table = hits();
        assert_eq!(table.column_index("userid"), Some(0));
        assert_eq!(table.column_index("SEARCHPHRASE"), Some(1));
        assert_eq!(table.column_index("nope"), None);
    }

    #[test]
    fn two_columns_with_one_name_is_caught() {
        let error = Table::new(
            QualifiedName::new("memory", "main", "t"),
            vec![Field::new("a", LogicalType::Integer), Field::new("A", LogicalType::Varchar)],
        )
        .expect_err("two columns called a");
        // Named after the second of the two and spelled the way it was written there, which is what
        // duckdb v1.4.1 says for `CREATE TABLE t (a INTEGER, A VARCHAR)`.
        assert_eq!(error.to_string(), "Catalog Error: Column with name A already exists!");
    }

    #[test]
    fn a_new_table_is_empty_and_typed() {
        let mut table = hits();
        assert!(table.rows().is_empty());
        assert_eq!(table.rows().types(), table.types());
        table
            .rows_mut()
            .append_rows(&[vec![Value::BigInt(1), Value::Varchar("a".to_string())]])
            .expect("a row of the table's own types");
        assert_eq!(table.rows().len(), 1);
    }

    /// A table whose first column refuses nulls and whose second does not.
    fn required() -> Table {
        Table::new(
            QualifiedName::new("memory", "main", "hits"),
            vec![
                Field::required("UserID", LogicalType::BigInt),
                Field::new("SearchPhrase", LogicalType::Varchar),
            ],
        )
        .expect("two columns with different names")
    }

    #[test]
    fn a_null_in_a_not_null_column_is_refused() {
        let mut table = required();
        let error = table
            .append_rows(&[vec![Value::Null, Value::Varchar("a".to_string())]])
            .expect_err("a null in UserID");
        assert_eq!(error.message(), "NOT NULL constraint failed: hits.UserID");
        assert!(table.rows().is_empty(), "the row was kept anyway");
    }

    #[test]
    fn a_null_in_a_column_that_allows_them_is_kept() {
        let mut table = required();
        table.append_rows(&[vec![Value::BigInt(7), Value::Null]]).expect("a null in SearchPhrase");
        assert_eq!(table.rows().len(), 1);
    }

    #[test]
    fn a_chunk_is_checked_through_its_mask() {
        let mut table = required();
        let phrase = Vector::constant(LogicalType::Varchar, Value::Varchar("a".to_string()), 2);
        let good = Chunk::new(vec![
            Vector::from_values(LogicalType::BigInt, &[Value::BigInt(1), Value::BigInt(2)])
                .expect("two ids"),
            phrase.clone(),
        ])
        .expect("two columns of two rows");
        table.append(good).expect("no nulls anywhere");
        let bad = Chunk::new(vec![
            Vector::from_values(LogicalType::BigInt, &[Value::BigInt(1), Value::Null])
                .expect("an id and a null"),
            phrase,
        ])
        .expect("two columns of two rows");
        let error = table.append(bad).expect_err("a null in UserID");
        assert_eq!(error.message(), "NOT NULL constraint failed: hits.UserID");
        assert_eq!(table.rows().len(), 2, "the bad chunk was kept anyway");
    }

    #[test]
    fn a_null_hiding_in_a_constant_is_found() {
        let mut table = required();
        let chunk = Chunk::new(vec![
            Vector::constant(LogicalType::BigInt, Value::Null, 4),
            Vector::constant(LogicalType::Varchar, Value::Varchar("a".to_string()), 4),
        ])
        .expect("two columns of four rows");
        let error = table.append(chunk).expect_err("a constant null in UserID");
        assert_eq!(error.message(), "NOT NULL constraint failed: hits.UserID");
    }

    #[test]
    fn a_null_behind_a_dictionary_code_is_found() {
        let mut table = required();
        let values = Vector::from_values(LogicalType::BigInt, &[Value::BigInt(1), Value::Null])
            .expect("an id and a null");
        let phrase = Vector::constant(LogicalType::Varchar, Value::Varchar("a".to_string()), 3);
        let good = Chunk::new(vec![
            Vector::dictionary(vec![0, 0, 0], values.clone()).expect("three rows"),
            phrase.clone(),
        ])
        .expect("two columns of three rows");
        table.append(good).expect("no row points at the null");
        let bad = Chunk::new(vec![
            Vector::dictionary(vec![0, 1, 0], values).expect("three rows"),
            phrase,
        ])
        .expect("two columns of three rows");
        let error = table.append(bad).expect_err("a null in UserID through its code");
        assert_eq!(error.message(), "NOT NULL constraint failed: hits.UserID");
        assert_eq!(table.rows().len(), 3, "the bad chunk was kept anyway");
    }
}
