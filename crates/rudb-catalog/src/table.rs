//! A table: a name, some columns, and the rows.

use std::sync::Arc;

use rudb_common::bounds::{Bound, Frequencies, Zones};
use rudb_common::stat::{Provenance, Stat};
use rudb_common::{Clustering, Error, Field, LogicalType, Result, Value};
use rudb_native::{
    Common, FrequencyOccurrences, FrequencyPrefix, PairFrequencyCounts, Reader as NativeReader,
    StoredPart, Stripes,
};
use rudb_storage::{MemoryTable, Probe};
use rudb_vector::{Chunk, Form, VECTOR_SIZE, Vector, concat};

use crate::catalog::DETACHED;
use crate::held::Held;
use crate::keys::{ForeignKey, Key, Seen};
use crate::name::{QualifiedName, same_name};

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
    /// added up. This is also why a checkpoint rewrites the file rather than carrying the table
    /// forward: [`Rows::is_native`] is false here, so the table is one the file and the catalog
    /// disagree about and the honest answer is to write it again.
    Grown(NativeReader, MemoryTable),
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

impl Rows {
    /// The rows to append to, turning a committed file into one that has rows in memory beside it.
    ///
    /// # Errors
    ///
    /// Never in practice. The branch that would report one is the committed file that was replaced
    /// on the line above, which the compiler cannot see is gone.
    pub fn to_append(&mut self) -> Result<&mut MemoryTable> {
        if let Self::Native(reader) = self {
            let types = reader.table().fields().iter().map(|field| field.ty.clone()).collect();
            *self = Self::Grown(reader.clone(), MemoryTable::new(types));
        }
        match self {
            Self::Memory(rows) | Self::Grown(_, rows) => Ok(rows),
            Self::Native(_) => Err(Error::internal("a committed table took no append buffer")),
        }
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
            Self::Memory(_) | Self::Grown(..) => None,
        }
    }

    /// How many stripes the committed file contributes, which the row groups are numbered after.
    ///
    /// The parts have the same split and are counted inline, because the reader is already in hand
    /// at every one of those and this one is asked where it is not.
    fn stripes_in_file(&self) -> usize {
        match self {
            Self::Memory(_) => 0,
            Self::Native(reader) | Self::Grown(reader, _) => reader.stripe_parts().len(),
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
            Self::Grown(_, _) => Ok(None),
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
            Self::Grown(_, _) => Ok(None),
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
            Self::Grown(_, _) => Ok(None),
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
            Self::Grown(_, _) => Ok(None),
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
            Self::Grown(_, _) => Ok(None),
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
            Self::Grown(_, _) => Ok(None),
        }
    }

    /// The sum of one integer column and the rows that went into it, from the zone maps of an in
    /// memory table or the directory of a file.
    pub fn exact_sum(&self, column: usize) -> Result<Option<(i128, u64)>> {
        match self {
            Self::Memory(rows) => rows.exact_sum(column),
            Self::Native(reader) => reader.exact_sum(column),
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
            Self::Grown(_, _) => Ok(None),
        }
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
            Self::Grown(reader, rows) => {
                if at < reader.parts() {
                    reader.part_rows(at)
                } else {
                    rows.chunk_len(at - reader.parts())
                        .ok_or_else(|| Error::internal("row ordinal names a missing chunk"))?
                }
            }
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
            let held = if dense {
                reader.read(part, columns)?
            } else {
                reader.read_sparse(part, columns)?
            };
            let selected = locations[from..upto]
                .iter()
                .map(|&(_, row)| {
                    u32::try_from(row)
                        .map_err(|_| Error::internal("a row within a part exceeds u32"))
                })
                .collect::<Result<Vec<_>>>()?;
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
            Self::Native(reader) | Self::Grown(reader, _) => {
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

    /// Number of independently readable chunks or parts.
    #[must_use]
    pub fn chunk_count(&self) -> usize {
        match self {
            Self::Memory(rows) => rows.chunk_count(),
            Self::Native(reader) => reader.parts(),
            Self::Grown(reader, rows) => reader.parts().saturating_add(rows.chunk_count()),
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
            Self::Grown(reader, rows) => {
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
            Self::Native(reader) | Self::Grown(reader, _) => reader.keep_stripes(stripes),
        }
    }

    /// Reads only projected columns.
    pub fn read(&self, at: usize, columns: &[usize]) -> Result<Chunk> {
        match self {
            Self::Memory(rows) => rows.read(at, columns),
            Self::Native(reader) => reader.read(at, columns),
            Self::Grown(reader, rows) => {
                if at < reader.parts() {
                    reader.read(at, columns)
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
            Self::Grown(reader, _) if at < reader.parts() => {
                reader.read_rows(at, columns, positions, whole)
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

    /// Whether statistics prove this chunk cannot match.
    #[must_use]
    pub fn skips(&self, at: usize, probes: &[Probe]) -> bool {
        match self {
            Self::Memory(rows) => rows.skips(at, probes),
            Self::Native(reader) => reader.skips(at, probes),
            Self::Grown(reader, rows) => {
                if at < reader.parts() {
                    reader.skips(at, probes)
                } else {
                    rows.skips(at - reader.parts(), probes)
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
            Self::Grown(_, _) => None,
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
            Self::Grown(_, _) => None,
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
            Self::Memory(_) | Self::Grown(_, _) => Vec::new(),
            // A reader that cannot answer its own directory is a reader that will fail the scan a
            // moment later with the same error, and the planner is not the place to raise it. An
            // empty list reads back as a table nobody counted, which is where this started.
            Self::Native(reader) => rudb_native::distincts(reader).unwrap_or_default(),
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
            Self::Native(reader) => rudb_native::ascending(reader),
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
            Self::Grown(reader, rows) => {
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
    /// The SQL of each `CHECK` constraint, in the order written.
    checks: Vec<String>,
    /// The foreign keys this table's rows have to meet, in the order written.
    foreign: Vec<ForeignKey>,
    /// The sequences its defaults call `nextval` on, which it depends on the way the pin records it:
    /// a `DROP SEQUENCE` without `CASCADE` is refused while this table is there.
    sequences: Vec<QualifiedName>,
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
            defaults: Vec::new(),
            checks: Vec::new(),
            foreign: Vec::new(),
            sequences: Vec::new(),
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
        Ok(Self {
            name,
            columns,
            rows: Rows::Native(reader),
            oid: DETACHED,
            clustering,
            keys: Vec::new(),
            seen: Vec::new(),
            defaults: Vec::new(),
            checks: Vec::new(),
            foreign: Vec::new(),
            sequences: Vec::new(),
        })
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
            Rows::Native(reader) | Rows::Grown(reader, _) => reader.stored(column),
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
            Rows::Native(reader) => reader.table().clustering() == self.clustering.as_ref(),
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
        if reader.table().rows() != self.rows.len() {
            return Err(Error::internal("a committed native snapshot changed its row count"));
        }
        // The file is the record, so the declaration comes back from it rather than being kept
        // from before. If the checkpoint did not write what this table asked for, this is where
        // that shows up, as the declaration going away rather than as a claim nothing backs.
        self.clustering = reader.table().clustering().cloned();
        self.rows = Rows::Native(reader);
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
        if !self.keys.is_empty() {
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
        let seen = self.appended_keys(&chunks)?;
        self.rows.to_append()?.append_all(chunks, workers)?;
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
            .keys
            .iter()
            .map(|key| Seen::of(&chunks, key, &self.columns, true))
            .collect::<Result<Vec<_>>>()?;
        let types = self.columns.iter().map(|field| field.ty.clone()).collect();
        let mut rows = MemoryTable::new(types);
        rows.append_all(chunks, workers)?;
        self.rows = Rows::Memory(rows);
        self.hold_keys(seen);
        Ok(())
    }

    /// The primary key and the unique constraints.
    #[must_use]
    pub fn keys(&self) -> &[Key] {
        &self.keys
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
        self.seen = vec![None; self.keys.len()];
        let seen = self.appended_keys(&[])?;
        self.hold_keys(seen);
        Ok(())
    }

    /// The key sets the table holds once these rows are appended, or the refusal of the first key
    /// they repeat. Builds the set of a key from the rows already held the first time it is asked.
    fn appended_keys(&mut self, chunks: &[Chunk]) -> Result<Vec<Seen>> {
        if self.keys.is_empty() {
            return Ok(Vec::new());
        }
        let mut sets = Vec::with_capacity(self.keys.len());
        for at in 0..self.keys.len() {
            let held = match &self.seen[at] {
                Some(held) => held.clone(),
                None => {
                    let all: Vec<usize> = (0..self.columns.len()).collect();
                    let mut stored = Vec::with_capacity(self.rows.chunk_count());
                    for chunk in 0..self.rows.chunk_count() {
                        stored.push(self.rows.read(chunk, &all)?);
                    }
                    let held = Seen::of(&stored, &self.keys[at], &self.columns, true)?;
                    self.seen[at] = Some(held.clone());
                    held
                }
            };
            sets.push(held.with(chunks, &self.keys[at], &self.columns)?);
        }
        Ok(sets)
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

    /// Adds rows of single values, refusing a null in a column that said it would not have one.
    ///
    /// # Errors
    ///
    /// If a row is not as wide as the table, if a value will not convert to its column's type, or
    /// if a `NOT NULL` column is handed a null.
    pub fn append_rows(&mut self, rows: &[Vec<Value>]) -> Result<()> {
        if !self.keys.is_empty() {
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
            let found = match vector.form() {
                Form::Flat | Form::Sequence => {
                    vector.validity().has_nulls(vector.len())
                        && (0..vector.len()).any(|row| !vector.validity().is_valid(row))
                }
                _ => (0..vector.len()).any(|row| vector.value_at(row).is_null()),
            };
            if found {
                return Err(self.null_in(&column.name));
            }
        }
        Ok(())
    }

    /// The error DuckDB raises when a null reaches a column that refuses them.
    fn null_in(&self, column: &str) -> Error {
        Error::constraint(format!("NOT NULL constraint failed: {}.{}", self.name.table, column))
    }
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
}
