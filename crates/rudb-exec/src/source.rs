//! The operators that produce rows without an input: the scan, the dummy and the literal rows.
//!
//! These are the bottom of every pipeline and they are all [`Source`] implementations, which is a
//! different shape from the operators above them. A source is shared rather than owned: one object
//! answers [`Source::morsel`] for every thread running the pipeline, so the position it is up to
//! lives in an atomic rather than in a field somebody mutates. What a morsel covers is the source's
//! own business, and the four here mean four different things by it, which is why the type carries
//! numbers and not rows.

use std::collections::HashMap;
use std::ops::Range;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use rudb_catalog::Table;
use rudb_common::{Error, Field, LogicalType, Result};
use rudb_csv::Reader as CsvReader;
use rudb_functions::{
    FILE_ROW_NUMBER, Given, TableFunction, csv_given, open_csv, open_parquet, series_length,
};
use rudb_kernels::cast;
use rudb_metrics::Counters;
use rudb_parquet::{Bound, Op, Reader, Test, skips};
use rudb_pipeline::{Morsel, Progress, Source};
use rudb_plan::{ExprRef, Plan, Slice};
use rudb_storage::Probe;
use rudb_vector::{Chunk, Data, VECTOR_SIZE, Vector};

use crate::expr::evaluate_all;
use crate::schema::Schema;

/// One morsel per position, handed to whoever asks first.
///
/// The three sources that already have their chunks, or can read one by number, hand out a morsel
/// per chunk, and this is the whole of the sharing that needs: a counter, one fetch and add per
/// morsel, and no lock held while anything is read. It is what makes the difference between two
/// threads scanning a table and two threads waiting for each other.
#[derive(Debug)]
pub(crate) struct Handout {
    next: AtomicU64,
    total: u64,
}

impl Handout {
    /// A handout over `total` positions.
    pub(crate) fn new(total: usize) -> Self {
        Self { next: AtomicU64::new(0), total: u64::try_from(total).unwrap_or(u64::MAX) }
    }

    /// How many positions there are in all, which is how many morsels this will ever hand out.
    pub(crate) fn total(&self) -> usize {
        usize::try_from(self.total).unwrap_or(usize::MAX)
    }

    /// The next position, as a morsel covering it, or `None` when they are all taken.
    pub(crate) fn take(&self) -> Option<Morsel> {
        let at = self.next.fetch_add(1, Ordering::Relaxed);
        (at < self.total).then(|| Morsel::new(at, at, at + 1))
    }
}

/// Where a morsel has got to, as an index into whatever the source counts.
pub(crate) fn position(morsel: &Morsel) -> usize {
    usize::try_from(morsel.cursor()).unwrap_or(usize::MAX)
}

fn poisoned<T>(_: T) -> Error {
    Error::internal("a thread panicked while reading a file")
}

/// A base table scan.
///
/// `columns` is the position in the stored table of each column the plan asked for, worked out once
/// when the operator is built. The plan's projection is a list of fields and the table's columns are
/// a list of fields, and they are the same list today only because the binder projects every column
/// in order. Resolving by name rather than assuming that is what keeps this operator correct after
/// projection pushdown makes the plan's list a subset, which is the M1 change section 9.2 describes
/// as the difference between 20 GB and 200 MB on ClickBench.
///
/// A morsel here is one stored chunk, because that is the unit the table hands back and reading
/// half of one costs the same as reading all of it. When the storage format's blocks are what is
/// scanned rather than an in memory table, a morsel becomes a run of rows inside a block and the
/// only thing that changes is what the numbers in it mean.
///
/// `probes` is what the filter above this scan already knows, in the same shape [`FileScan`] takes
/// it, and it is answered against the table's zone maps a chunk at a time. That is a finer unit than
/// the Parquet path gets: a row group on the files this engine is measured against is a hundred
/// thousand rows and a chunk is two thousand and forty eight, and on a selective filter over a
/// clustered column that is most of the difference between the two paths.
#[derive(Debug)]
pub(crate) struct Scan<'a> {
    table: &'a Table,
    columns: Vec<usize>,
    probes: Vec<Probe>,
    schema: Schema,
    chunks: Handout,
    skipped: AtomicUsize,
}

impl<'a> Scan<'a> {
    /// A scan of `table` producing the plan's projected columns.
    ///
    /// # Errors
    ///
    /// If the plan asks for a column the table does not have, which means the catalog changed under
    /// a plan that was bound against it.
    pub(crate) fn new(
        plan: &Plan,
        table: &'a Table,
        index: u32,
        projection: Slice,
        tests: Vec<(usize, Op, Bound)>,
    ) -> Result<Self> {
        let fields = plan.field_list(projection).to_vec();
        let mut columns = Vec::with_capacity(fields.len());
        for field in &fields {
            let position = table.column_index(&field.name).ok_or_else(|| {
                Error::catalog(format!(
                    "Table \"{}\" does not have a column named \"{}\"",
                    table.name().table,
                    field.name
                ))
            })?;
            columns.push(position);
        }
        // A test names a column of the projection and a zone names a column of the table, so the
        // test is moved onto the table's numbering here rather than at every chunk. A test on a
        // column that is somehow not projected is dropped, which costs a chunk that gets read.
        let probes = tests
            .into_iter()
            .filter_map(|(at, op, value)| Some(Probe { column: *columns.get(at)?, op, value }))
            .collect();
        let schema = Schema::numbered(fields, index);
        let chunks = Handout::new(table.rows().chunk_count());
        Ok(Self { table, columns, probes, schema, chunks, skipped: AtomicUsize::new(0) })
    }

    /// What this scan produces.
    pub(crate) fn schema(&self) -> &Schema {
        &self.schema
    }
}

impl Source for Scan<'_> {
    fn morsel(&self) -> Option<Morsel> {
        self.chunks.take()
    }

    fn morsels(&self) -> Option<usize> {
        Some(self.chunks.total())
    }

    fn read(&self, morsel: &mut Morsel, out: &mut Chunk) -> Result<Progress> {
        let at = position(morsel);
        if at >= self.table.rows().chunk_count() {
            *out = Chunk::empty(&self.schema.types());
            return Ok(Progress::Done);
        }
        morsel.advance(1);
        // A chunk the zone maps have ruled out is never read, so its columns are never copied and
        // its rows are never handed to the filter above. An empty chunk is what the rest of the
        // pipeline already expects from a morsel with nothing in it.
        if !self.probes.is_empty() && self.table.rows().skips(at, &self.probes) {
            self.skipped.fetch_add(1, Ordering::Relaxed);
            *out = Chunk::empty(&self.schema.types());
            return Ok(Progress::Done);
        }
        *out = self.table.rows().read(at, &self.columns)?;
        Ok(Progress::Done)
    }
}

/// One row and no columns.
///
/// What `SELECT 1` sits on. It produces a chunk of width zero and length one exactly once, which is
/// the case `Chunk`'s stored row count exists for.
#[derive(Debug)]
pub(crate) struct Dummy {
    schema: Schema,
    one: Handout,
}

impl Dummy {
    pub(crate) fn new() -> Self {
        Self { schema: Schema::empty(), one: Handout::new(1) }
    }

    /// What this produces, which is no columns at all.
    pub(crate) fn schema(&self) -> &Schema {
        &self.schema
    }
}

impl Source for Dummy {
    fn morsel(&self) -> Option<Morsel> {
        self.one.take()
    }

    fn morsels(&self) -> Option<usize> {
        Some(self.one.total())
    }

    fn read(&self, morsel: &mut Morsel, out: &mut Chunk) -> Result<Progress> {
        *out = Chunk::with_rows(Vec::new(), 1)?;
        morsel.advance(1);
        Ok(Progress::Done)
    }
}

/// Literal rows.
///
/// The expressions are evaluated once when the operator is built, over a one row chunk with no
/// columns, because a `VALUES` row in a bound plan is constants and folded arithmetic and cannot
/// refer to anything. Evaluating them lazily would buy nothing and would make an error in a literal
/// arrive on the first `next` rather than where the query says it is.
#[derive(Debug)]
pub(crate) struct Values {
    schema: Schema,
    chunks: Vec<Chunk>,
    handout: Handout,
}

impl Values {
    /// The rows of a [`Node::Values`](rudb_plan::Node::Values), already evaluated.
    ///
    /// # Errors
    ///
    /// If a row is not as wide as the column list, or anything the expressions report.
    pub(crate) fn new(plan: &Plan, index: u32, columns: Slice, rows: Slice) -> Result<Self> {
        let fields = plan.field_list(columns).to_vec();
        let schema = Schema::numbered(fields, index);
        let types = schema.types();
        let source = Schema::empty();
        let one = Chunk::with_rows(Vec::new(), 1)?;
        let mut down: Vec<Vec<rudb_common::Value>> = vec![Vec::new(); types.len()];
        for row in plan.row_list(rows) {
            let exprs: Vec<ExprRef> = plan.expr_list(*row).to_vec();
            if exprs.len() != types.len() {
                return Err(Error::internal(format!(
                    "a VALUES row of {} expressions in a {} column list",
                    exprs.len(),
                    types.len()
                )));
            }
            let evaluated = evaluate_all(plan, &exprs, &source, &one)?;
            for (position, vector) in evaluated.iter().enumerate() {
                down[position].push(vector.value_at(0));
            }
        }
        let total = down.first().map_or(0, Vec::len);
        let mut chunks = Vec::new();
        let mut start = 0;
        while start < total {
            let end = (start + VECTOR_SIZE).min(total);
            let mut built = Vec::with_capacity(types.len());
            for (position, ty) in types.iter().enumerate() {
                built.push(Vector::from_values(ty.clone(), &down[position][start..end])?);
            }
            chunks.push(Chunk::with_rows(built, end - start)?);
            start = end;
        }
        let handout = Handout::new(chunks.len());
        Ok(Self { schema, chunks, handout })
    }

    /// What these rows are.
    pub(crate) fn schema(&self) -> &Schema {
        &self.schema
    }
}

/// A table function that produces a run of integers.
///
/// The arguments are evaluated once when the operator is built, the same way a `VALUES` row is and
/// for the same reason: they are constants by the time they are here, since a table function that
/// can see a row is `LATERAL` and does not bind to this node.
///
/// The values are produced a chunk at a time rather than all at once. `range(100000000)` is a
/// hundred million rows and a corpus that writes it means it, so materializing the whole run into
/// a `Vec` before the first chunk comes out would be eight hundred megabytes for a query whose
/// answer is one number.
/// A morsel here is a run of positions in the sequence, several chunks long, because the rows are
/// worked out rather than read and handing out a morsel per chunk would be more counter traffic than
/// arithmetic. Sixteen chunks is small enough that a hundred million rows is still six thousand
/// units for a scheduler to balance and large enough that the handout is not the cost.
#[derive(Debug)]
pub(crate) struct Series {
    schema: Schema,
    /// The first value, which is the value at position zero.
    start: i64,
    step: i64,
    /// How many values there are, which `series_length` worked out once.
    rows: u64,
    morsels: AtomicU64,
}

/// How many positions one morsel of a series covers.
const RUN: u64 = 16 * VECTOR_SIZE as u64;

impl Series {
    /// The rows of a [`Node::TableFunction`](rudb_plan::Node::TableFunction).
    ///
    /// A null in any argument gives no rows at all, which is DuckDB's answer and is not the same
    /// as an error. The three defaults are the three that make a one argument call mean what
    /// everybody writes it to mean, which is zero up to the number.
    ///
    /// # Errors
    ///
    /// Whatever evaluating an argument reports, and a step of zero.
    pub(crate) fn new(plan: &Plan, index: u32, function: &str, args: Slice) -> Result<Self> {
        let Some(function) = TableFunction::lookup(function) else {
            return Err(Error::internal(format!("a plan with a table function called {function}")));
        };
        let fields = vec![Field::new(function.name(), LogicalType::BigInt)];
        let schema = Schema::numbered(fields, index);

        let exprs: Vec<ExprRef> = plan.expr_list(args).to_vec();
        let source = Schema::empty();
        let one = Chunk::with_rows(Vec::new(), 1)?;
        let evaluated = evaluate_all(plan, &exprs, &source, &one)?;
        let mut given = Vec::with_capacity(evaluated.len());
        for vector in &evaluated {
            match vector.value_at(0) {
                rudb_common::Value::Null => return Ok(Self::empty(schema)),
                rudb_common::Value::BigInt(n) => given.push(n),
                other => {
                    return Err(Error::internal(format!(
                        "a table function argument bound as BIGINT arrived as {other}"
                    )));
                }
            }
        }
        let (start, stop, step) = match given.as_slice() {
            [stop] => (0, *stop, 1),
            [start, stop] => (*start, *stop, 1),
            [start, stop, step] => (*start, *stop, *step),
            _ => {
                return Err(Error::internal(format!(
                    "{}() bound with {} arguments",
                    function.name(),
                    given.len()
                )));
            }
        };
        let rows = u64::try_from(series_length(function, start, stop, step)?).unwrap_or(u64::MAX);
        Ok(Self { schema, start, step, rows, morsels: AtomicU64::new(0) })
    }

    fn empty(schema: Schema) -> Self {
        Self { schema, start: 0, step: 1, rows: 0, morsels: AtomicU64::new(0) }
    }

    /// What this produces, which is one BIGINT column named after the function.
    pub(crate) fn schema(&self) -> &Schema {
        &self.schema
    }

    /// The value at a position in the sequence.
    ///
    /// Worked out rather than carried, because a thread that is handed the tenth morsel has not
    /// counted its way to it and never will. Inside a chunk the step is still added a row at a time,
    /// which is what keeps the answers the same as the loop that used to be here.
    fn value_at(&self, position: u64) -> i64 {
        let steps = i64::try_from(position).unwrap_or(i64::MAX);
        self.start.saturating_add(self.step.saturating_mul(steps))
    }
}

impl Source for Series {
    fn morsel(&self) -> Option<Morsel> {
        let index = self.morsels.fetch_add(1, Ordering::Relaxed);
        let start = index.saturating_mul(RUN);
        (start < self.rows)
            .then(|| Morsel::new(index, start, self.rows.min(start.saturating_add(RUN))))
    }

    fn morsels(&self) -> Option<usize> {
        Some(usize::try_from(self.rows.div_ceil(RUN)).unwrap_or(usize::MAX))
    }

    fn read(&self, morsel: &mut Morsel, out: &mut Chunk) -> Result<Progress> {
        let count = usize::try_from(morsel.remaining()).unwrap_or(usize::MAX).min(VECTOR_SIZE);
        if count == 0 {
            *out = Chunk::empty(&[LogicalType::BigInt]);
            return Ok(Progress::Done);
        }
        // The loop is over `i64` rather than over `Value`, and the vector is built out of the run
        // it fills rather than out of a list of tagged values that would have to be read back one
        // at a time to find the run again. `range()` is the source every microbenchmark in
        // `rudb-bench` reads from, so a chunk of it costing a `Value` a row would be measuring the
        // generator instead of what is downstream of it.
        let mut at = self.value_at(morsel.cursor());
        let mut counted = Vec::with_capacity(count);
        for _ in 0..count {
            counted.push(at);
            at = at.saturating_add(self.step);
        }
        morsel.advance(u64::try_from(count).unwrap_or(u64::MAX));
        let vector = Vector::flat(LogicalType::BigInt, Data::Int64(counted.into()))?;
        *out = Chunk::with_rows(vec![vector], count)?;
        Ok(if morsel.is_drained() { Progress::Done } else { Progress::More })
    }
}

/// A scan of one or more files, Parquet or CSV.
///
/// The files are named by the arguments, which the binder already expanded: a pattern was walked
/// there and a name that is not a pattern was checked there, so what arrives is a list of names that
/// existed when the statement was bound. They are opened here rather than being carried from the
/// binder, because binding and running are separated by however long a prepared statement lives and
/// a plan that held open descriptors would hold them for all of that.
///
/// One at a time, in the order the list gives, which is the order the rows come out in. Opening all
/// of them up front would mean a directory of ten thousand files costing ten thousand descriptors
/// before the first row, and closing each one at its end is what makes a scan of a whole directory
/// cost one.
///
/// The plan's column list is resolved against each file's by name, which is the same thing [`Scan`]
/// does against a catalog table and for the same reason. Today the binder projects every column in
/// order, so the mapping is the identity, and the moment projection pushdown makes the plan's list a
/// subset the reader reads a subset. That is the difference `spec/engine/05-scan.md` section 5.6
/// describes between reading two columns of ClickBench and reading a hundred and five.
///
/// The first file decides the types and every file after it is cast to them, which is DuckDB's rule
/// and was measured: a second file holding `'5'` where the first holds an `INTEGER` reads as 5, and
/// one holding `'txt'` is a conversion error naming the file it came from.
///
/// That is the Parquet rule and CSV does not follow it. A Parquet file states its schema, so there
/// is a first file's word to take, and a CSV file states nothing, so the binder sniffed all of them
/// and combined the answers. What arrives here is that combined answer, and each CSV file is told it
/// as it is opened rather than being allowed to use its own sample, which is what keeps a file that
/// happens to hold nothing but whole numbers from handing up BIGINT into a stream that is DOUBLE.
///
/// A morsel is one row group of one Parquet file, or one whole CSV file.
///
/// The row group is what the format stores and what a reader can be positioned at without having
/// read what came before it, so it is the smallest unit two threads can take without one of them
/// waiting on the other. A CSV file cannot be positioned at all, because nothing in it says where a
/// row begins until every byte before it has been parsed, so a CSV morsel is a whole file and a
/// query over one CSV file reads it on one thread.
///
/// The files are cut into morsels one file at a time rather than all at once. Cutting a file means
/// reading its footer, and a directory of ten thousand files would be ten thousand footers read
/// before the first row came out, which is what the paragraph above about descriptors is about and
/// is the same answer.
///
/// Each morsel carries its own reader, and for Parquet those readers share one open file and one
/// parsed footer through [`Reader::split`]. So the scan holds no lock across a read, which is the
/// whole point: the version of this before #486 had one morsel, one reader and a mutex around it,
/// and a second thread asking for work got none.
#[derive(Debug)]
pub(crate) struct FileScan {
    function: TableFunction,
    paths: Vec<String>,
    given: Given,
    wanted: Vec<Field>,
    /// Whether the last column the scan produces is the row's ordinal inside its own file.
    ///
    /// `file_row_number=True`, which is a column no file holds and the scan counts. It is last
    /// because the binder puts it last, and it is a flag rather than a position because everything
    /// else here indexes [`Self::wanted`] and that list is the file's columns only.
    numbered: bool,
    schema: Schema,
    /// The comparisons a row group's bounds can be checked against before it is handed out.
    ///
    /// Written in terms of this scan's own output positions, because that is what the filter above
    /// it is written in terms of and what the builder can read without knowing which file is open.
    /// [`Self::advance`] turns them into the file's column numbers, once per file, since two files
    /// of one glob are allowed to hold the same columns in a different order.
    ///
    /// Empty when there is no filter above the scan, when the filter has no conjunct a bound can
    /// answer, or when the source is a CSV, and empty means every row group is handed out, which is
    /// what every scan did before this existed.
    tests: Vec<(usize, Op, Bound)>,
    /// How far through the file list the cutting has got, and the file it is in the middle of.
    cutting: Mutex<Cutting>,
    /// What each morsel handed out covers, by [`Morsel::index`].
    ///
    /// The outer lock is held for a lookup and a clone of one handle, and the read that follows
    /// holds the inner one, so two threads reading two row groups never wait for each other. The
    /// entries stay after their morsel is drained, because they are three words and a dropped
    /// reader once the rows are out and because a driver is allowed to ask again.
    open: Mutex<HashMap<u64, Arc<Mutex<Piece>>>>,
    counters: Option<Arc<Counters>>,
}

/// How many rows a scan aims to put in one morsel.
///
/// A morsel is the unit of work a thread takes, so it decides two things at once: how many threads
/// can be busy at all, and how evenly the last round of work divides among them. A row group is the
/// obvious unit and it is the wrong size for both. DuckDB writes a hundred and twenty two thousand
/// rows into one, so the million row ClickBench file has nine, and nine pieces of work is nine busy
/// threads on a machine with thirty two and one straggler deciding when everybody is finished.
///
/// Thirty two thousand is measured rather than picked. The same suite over copies of that file
/// written with different row group sizes runs in 898 ms on nine groups, 749 on thirty one and 768
/// on sixty one, all at eight threads, and at thirty two threads the nine group file is slower than
/// it was at eight while the thirty one group file is faster again at 676.
const MORSEL_ROWS: usize = 32_768;

/// The rows of the next morsel of a row group of `rows` rows, `part` of which are handed out.
///
/// Even pieces rather than full ones and a remainder, because the remainder is the piece everybody
/// else waits for. A group of a hundred and twenty three thousand rows is four morsels of thirty one
/// thousand rather than three of thirty two thousand and one of twenty five.
///
/// An empty range means the group is done, and a group of no rows is done straight away, which is
/// what stops a file with an empty row group in it from being cut forever.
fn next_piece(rows: usize, part: usize, target: usize) -> Range<usize> {
    let each = rows.div_ceil(rows.div_ceil(target.max(1)).max(1));
    let upto = part.saturating_add(each).min(rows);
    part.min(upto)..upto
}

/// How many morsels a row group of `rows` rows is cut into.
fn parts(rows: usize, target: usize) -> usize {
    rows.div_ceil(target.max(1)).max(1)
}

/// How many rows the row group at `at` holds, or none if it is not a group this file has.
fn group_rows(reader: &Reader, at: usize) -> usize {
    reader
        .metadata()
        .row_groups
        .get(at)
        .map_or(0, |group| usize::try_from(group.rows).unwrap_or(usize::MAX))
}

/// How many morsels a whole file comes to, which is what [`Source::morsels`] answers with.
fn pieces(reader: &FileReader) -> usize {
    let FileReader::Parquet(reader) = reader else { return 1 };
    reader
        .metadata()
        .row_groups
        .iter()
        .map(|group| parts(usize::try_from(group.rows).unwrap_or(usize::MAX), MORSEL_ROWS))
        .sum::<usize>()
        .max(1)
}

/// Where the cutting has got to.
///
/// One file's worth of morsels is cut at a time, and the reader the Parquet splits come off is kept
/// for as long as that file has row groups left to hand out.
#[derive(Debug)]
struct Cutting {
    /// How many files have been opened, which is the next one to open.
    at: usize,
    /// The reader the file being cut is read through, `None` between files.
    reader: Option<FileReader>,
    /// The next row group of that file to hand out, and one past its last.
    group: usize,
    groups: usize,
    /// How many rows of that row group have been handed out already.
    ///
    /// A row group is cut into several morsels when it is large enough to be worth cutting, so the
    /// cutting sits inside a group as well as between them. Zero whenever the next morsel starts a
    /// group, which is every morsel of a file whose groups are small.
    part: usize,
    /// How many morsels the whole of the file being cut comes to.
    ///
    /// Worked out once when the file is opened, because [`Source::morsels`] is asked before any of
    /// them is handed out and it is asked to decide how many instances of a pipeline to build.
    pieces: usize,
    /// [`FileScan::tests`] against the column numbers of the file being cut.
    skipping: Vec<Test>,
    /// How many row groups the bounds have ruled out so far, over every file of the scan.
    ///
    /// Nothing downstream needs this. It is here because a pruning that silently stops working
    /// costs time and nothing else, so the tests read it to prove groups are actually being
    /// skipped rather than read and filtered.
    skipped: usize,
    /// The ordinal in that file of the first row of the next morsel, which is what
    /// `file_row_number` counts from and what keeps that column right whatever order the morsels
    /// are read in.
    row: i64,
    /// How many morsels have been handed out, which is the next one's index.
    given: u64,
}

/// What one morsel covers, and the reader open on it.
#[derive(Debug)]
struct Piece {
    /// Which of the scan's paths, so that a column that will not cast names the file it came from.
    file: usize,
    /// The reader, taken away once it has no more chunks in it.
    reader: Option<FileReader>,
    /// What went wrong cutting this morsel, if anything.
    ///
    /// [`Source::morsel`] hands back an `Option` and has nowhere to put an error, and a file that
    /// cannot be opened half way through a scan has to be reported rather than read past. So the
    /// handout queues a morsel that covers nothing and carries the error, and the read reports it.
    failure: Option<Error>,
    /// The ordinal in the file of the next row this morsel produces.
    row: i64,
}

impl FileScan {
    /// The rows of a `read_parquet` or `read_csv` call, over every file it names.
    ///
    /// # Errors
    ///
    /// If the first file is gone or unreadable since it was bound, or if it no longer has a column
    /// the plan asked for, which is what a file replaced between binding and running looks like.
    ///
    /// All but the last of these are the plan and the fields of one node of it, and bundling them
    /// into a struct on the way in would only spell the same node a second way.
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn new(
        plan: &Plan,
        index: u32,
        function: TableFunction,
        args: Slice,
        options: Slice,
        settings: Slice,
        columns: Slice,
        tests: Vec<(usize, Op, Bound)>,
    ) -> Result<Self> {
        let paths = file_arguments(plan, args, function)?;
        let given = csv_options(plan, options, settings)?;
        let produced = plan.field_list(columns).to_vec();
        // The binder puts the counted column last and nothing between here and there reorders a
        // scan's columns, so the flag is whether the last one is it. Pruning can drop it, in which
        // case there is nothing to count, and pruning can drop everything else, in which case the
        // file is opened for its row count and no column of it is read.
        let numbered = produced.last().is_some_and(|field| field.name == FILE_ROW_NUMBER);
        let wanted =
            if numbered { produced[..produced.len() - 1].to_vec() } else { produced.clone() };
        let scan = Self {
            function,
            paths,
            given,
            wanted,
            numbered,
            schema: Schema::numbered(produced, index),
            tests,
            cutting: Mutex::new(Cutting {
                at: 0,
                reader: None,
                group: 0,
                groups: 0,
                part: 0,
                pieces: 1,
                skipping: Vec::new(),
                skipped: 0,
                row: 0,
                given: 0,
            }),
            open: Mutex::new(HashMap::new()),
            counters: None,
        };
        // The first file is opened now rather than on the first read, so that a file that has gone
        // missing since binding is reported where a caller is still asking a question about this
        // scan rather than in the middle of a result.
        {
            let mut cutting = scan.cutting.lock().map_err(poisoned)?;
            scan.advance(&mut cutting)?;
        }
        Ok(scan)
    }

    /// Connects this source's file counters to the operator row that owns it.
    pub(crate) fn watched(mut self, counters: Arc<Counters>) -> Self {
        self.counters = Some(counters);
        self
    }

    /// What this scan produces, in the types the first file settled on.
    pub(crate) fn schema(&self) -> &Schema {
        &self.schema
    }

    /// Opens the next file and projects it, or leaves the reader empty at the end of the list.
    fn advance(&self, cutting: &mut Cutting) -> Result<()> {
        cutting.reader = None;
        cutting.row = 0;
        cutting.group = 0;
        cutting.groups = 0;
        cutting.part = 0;
        cutting.pieces = 1;
        cutting.skipping = Vec::new();
        let Some(path) = self.paths.get(cutting.at) else { return Ok(()) };
        let mut reader = FileReader::open(self.function, path, self.given)?;
        let first = if cutting.at == 0 { None } else { self.paths.first().map(String::as_str) };
        let held = positions(self.function, &self.wanted, &reader.fields(), path, first)?;
        reader.project(&held)?;
        reader.settle(&self.wanted)?;
        cutting.groups = reader.row_groups();
        cutting.pieces = pieces(&reader);
        cutting.skipping = self
            .tests
            .iter()
            .filter_map(|(at, op, value)| {
                Some(Test { column: *held.get(*at)?, op: *op, value: value.clone() })
            })
            .collect();
        cutting.reader = Some(reader);
        cutting.at += 1;
        Ok(())
    }

    /// The next morsel's worth of the file being cut, or `None` when that file has none left.
    ///
    /// A Parquet file gives one per row group and takes a split of the reader it was opened with. A
    /// CSV file gives one, which takes the reader itself, because a CSV reader cannot be positioned
    /// and a second one over the same file would parse the same bytes to find the same rows.
    fn cut(&self, cutting: &mut Cutting) -> Result<Option<Piece>> {
        let file = cutting.at.saturating_sub(1);
        if let Some(FileReader::Parquet(reader)) = cutting.reader.as_ref() {
            // Row groups the filter above this scan has already ruled out are stepped over here
            // rather than handed out and thrown away downstream, which is the whole point: the data
            // pages of a skipped group are never read, never decompressed and never decoded. The row
            // counter still moves, because a later group's rows keep the numbers the file gives them.
            let metadata = reader.metadata();
            while cutting.group < cutting.groups {
                let Some(group) = metadata.row_groups.get(cutting.group) else { break };
                if !skips(&cutting.skipping, group, &metadata.schema) {
                    break;
                }
                cutting.group += 1;
                cutting.row = cutting.row.saturating_add(group.rows);
                cutting.skipped = cutting.skipped.saturating_add(1);
            }
        }
        let piece = match cutting.reader.as_ref() {
            Some(FileReader::Parquet(reader)) if cutting.group < cutting.groups => {
                let at = cutting.group;
                let rows = group_rows(reader, at);
                let piece = next_piece(rows, cutting.part, MORSEL_ROWS);
                let split = reader.split_rows(at, piece.clone())?;
                if piece.end >= rows {
                    cutting.group += 1;
                    cutting.part = 0;
                } else {
                    cutting.part = piece.end;
                }
                let row = cutting.row;
                cutting.row = cutting
                    .row
                    .saturating_add(i64::try_from(piece.end - piece.start).unwrap_or(i64::MAX));
                Piece { file, reader: Some(FileReader::Parquet(split)), failure: None, row }
            }
            Some(FileReader::Csv(_)) => {
                Piece { file, reader: cutting.reader.take(), failure: None, row: 0 }
            }
            // Either the row groups of the file being cut have all been handed out, or there is no
            // file being cut at all, and both mean the same thing to the caller.
            _ => return Ok(None),
        };
        Ok(Some(piece))
    }

    /// Registers a morsel's worth of work and hands back the morsel that covers it.
    fn hand(&self, cutting: &mut Cutting, piece: Piece) -> Option<Morsel> {
        let index = cutting.given;
        cutting.given += 1;
        let covers = u64::from(piece.failure.is_none());
        self.open.lock().ok()?.insert(index, Arc::new(Mutex::new(piece)));
        Some(Morsel::new(index, 0, covers))
    }

    /// What the morsel of this index covers.
    fn piece(&self, index: u64) -> Result<Arc<Mutex<Piece>>> {
        let open = self.open.lock().map_err(poisoned)?;
        open.get(&index)
            .map(Arc::clone)
            .ok_or_else(|| Error::internal(format!("a file scan was read at morsel {index}")))
    }

    /// The chunk with the row number column on the end of it.
    ///
    /// Built rather than read, because no file holds it. The values are a run, and the reason this
    /// is a loop over a range rather than a sequence vector is that the scan's consumer is free to
    /// slice or gather the chunk and a flat column survives both without a case.
    fn number(&self, chunk: Chunk, piece: &mut Piece) -> Result<Chunk> {
        let rows = chunk.len();
        let mut columns = Vec::with_capacity(chunk.width() + 1);
        for at in 0..chunk.width() {
            columns.push(chunk.column(at)?.clone());
        }
        let first = piece.row;
        piece.row = piece.row.saturating_add(i64::try_from(rows).unwrap_or(i64::MAX));
        let mut data = Vec::with_capacity(rows);
        for at in 0..rows {
            data.push(first.saturating_add(i64::try_from(at).unwrap_or(i64::MAX)));
        }
        columns.push(Vector::flat(LogicalType::BigInt, Data::Int64(data.into()))?);
        Chunk::with_rows(columns, rows)
    }

    /// The chunk with every column in the type the first file gave it.
    ///
    /// Almost always nothing, because almost always every file has the same schema, and the check is
    /// a type comparison per column per chunk rather than per row.
    ///
    /// `file` is which of the scan's paths this chunk came out of, and the only thing it is for is
    /// naming that file if a column will not cast.
    fn conform(&self, chunk: Chunk, file: usize) -> Result<Chunk> {
        let rows = chunk.len();
        let settled = self.wanted.iter().enumerate().all(|(at, field)| {
            chunk.column(at).is_ok_and(|column| column.logical_type() == &field.ty)
        });
        if settled {
            return Ok(chunk);
        }
        let mut columns = Vec::with_capacity(self.wanted.len());
        for (at, field) in self.wanted.iter().enumerate() {
            let column = chunk.column(at)?;
            if column.logical_type() == &field.ty {
                columns.push(column.clone());
                continue;
            }
            columns.push(cast(column, &field.ty, false).map_err(|error| {
                let path = self.paths.get(file).map_or("", String::as_str);
                Error::conversion(format!(
                    "Error while reading file \"{path}\": failed to cast column \"{}\" from type \
                     {} to {}: {}",
                    field.name,
                    column.logical_type(),
                    field.ty,
                    error.message()
                ))
            })?);
        }
        Chunk::with_rows(columns, rows)
    }
}

impl Source for FileScan {
    fn morsel(&self) -> Option<Morsel> {
        let mut cutting = self.cutting.lock().ok()?;
        loop {
            match self.cut(&mut cutting) {
                Ok(Some(piece)) => return self.hand(&mut cutting, piece),
                Ok(None) => {}
                Err(error) => {
                    let file = cutting.at.saturating_sub(1);
                    let piece = Piece { file, reader: None, failure: Some(error), row: 0 };
                    return self.hand(&mut cutting, piece);
                }
            }
            if cutting.at >= self.paths.len() {
                return None;
            }
            if let Err(error) = self.advance(&mut cutting) {
                let file = cutting.at.saturating_sub(1);
                let piece = Piece { file, reader: None, failure: Some(error), row: 0 };
                return self.hand(&mut cutting, piece);
            }
        }
    }

    /// The first file's morsels, taken as what every file in the list looks like.
    ///
    /// The first file is already open, because opening it is how a scan reports a file that has
    /// gone missing since it was bound, so what its row groups come to is there to be read without
    /// opening anything. The rest are assumed to match, which is right for a directory written by
    /// one writer and is the case worth being right about. A CSV file has one morsel however large
    /// it is, since a CSV reader cannot be positioned, so a list of CSV files is as many morsels as
    /// there are files.
    fn morsels(&self) -> Option<usize> {
        let cutting = self.cutting.lock().ok()?;
        Some(cutting.pieces.max(1).saturating_mul(self.paths.len().max(1)))
    }

    fn read(&self, morsel: &mut Morsel, out: &mut Chunk) -> Result<Progress> {
        let piece = self.piece(morsel.index())?;
        let mut piece = piece.lock().map_err(poisoned)?;
        if let Some(error) = piece.failure.take() {
            return Err(error);
        }
        loop {
            let file = piece.file;
            let Some(reader) = piece.reader.as_mut() else {
                morsel.advance(1);
                *out = Chunk::empty(&self.schema.types());
                return Ok(Progress::Done);
            };
            let before = reader.bytes_read();
            let next = reader.next_chunk()?;
            if let Some(counters) = &self.counters {
                counters.read(reader.bytes_read().saturating_sub(before));
            }
            let Some(chunk) = next else {
                // A file that is empty gives no chunk rather than an empty one, so this is not the
                // place that skips it. Taking the reader away and going round again is.
                piece.reader = None;
                continue;
            };
            let mut chunk = self.conform(chunk, file)?;
            if self.numbered {
                chunk = self.number(chunk, &mut piece)?;
            }
            *out = chunk;
            return Ok(Progress::More);
        }
    }
}

/// One open file, whichever of the two readers it needed.
///
/// An enum rather than a trait because there are two of them and they are both in this workspace.
/// What is behind the two is not alike at all, which is the reason the enum is here rather than the
/// readers being made to look the same: a Parquet file states its schema and stores each column
/// apart, so reading two of a hundred and five is reading two stretches of the file, while a CSV
/// file states nothing and interleaves everything, so every byte is parsed whatever the projection
/// is and the projection only saves the conversion and the copy. The three calls they do share are
/// exactly the three the scan above needs.
#[derive(Debug)]
enum FileReader {
    Parquet(Reader),
    Csv(CsvReader),
}

impl FileReader {
    /// Opens `path` with the reader `function` names.
    fn open(function: TableFunction, path: &str, given: Given) -> Result<Self> {
        match function {
            TableFunction::ReadCsv => Ok(Self::Csv(open_csv(path, given)?)),
            _ => Ok(Self::Parquet(open_parquet(path)?)),
        }
    }

    /// The columns the file holds, in the order it holds them.
    fn fields(&self) -> Vec<Field> {
        match self {
            Self::Parquet(reader) => reader.fields(),
            Self::Csv(reader) => reader.fields(),
        }
    }

    /// Reads only these columns, by position in the file, in this order.
    fn project(&mut self, columns: &[usize]) -> Result<()> {
        match self {
            Self::Parquet(reader) => reader.project(columns),
            Self::Csv(reader) => reader.project(columns),
        }
    }

    /// Tells the file the types the whole read settled on, where that is a thing to say.
    ///
    /// It is one thing for Parquet and everything for CSV. A Parquet file states its types and the
    /// first file's are the read's, so a later file that disagrees is read as what it holds and cast
    /// by [`FileScan::conform`]. The exception is a byte array column the plan wants as text, which
    /// is `binary_as_string` arriving as the answer it produced rather than as a flag of its own,
    /// and which is a rename rather than a conversion. A CSV file has no types of its own, only the
    /// ones a sample of it suggested, and the read's came from combining the samples of every file,
    /// so this replaces the suggestion before a row is parsed rather than converting twice.
    fn settle(&mut self, wanted: &[Field]) -> Result<()> {
        match self {
            Self::Parquet(reader) => {
                let text: Vec<bool> =
                    wanted.iter().map(|field| field.ty == LogicalType::Varchar).collect();
                reader.as_string(&text);
                Ok(())
            }
            Self::Csv(reader) => {
                let types: Vec<LogicalType> = wanted.iter().map(|field| field.ty.clone()).collect();
                reader.retype(&types)
            }
        }
    }

    /// The next chunk, or `None` at the end of the file.
    fn next_chunk(&mut self) -> Result<Option<Chunk>> {
        match self {
            Self::Parquet(reader) => reader.next_chunk(),
            Self::Csv(reader) => reader.next_chunk(),
        }
    }

    /// How many row groups the file has, which is how many morsels it is worth.
    ///
    /// Zero for CSV, which has none, and which the scan reads as one morsel covering the file.
    fn row_groups(&self) -> usize {
        match self {
            Self::Parquet(reader) => reader.metadata().row_groups.len(),
            Self::Csv(_) => 0,
        }
    }

    /// Compressed column bytes read so far, where the reader exposes that distinction.
    fn bytes_read(&self) -> u64 {
        match self {
            Self::Parquet(reader) => reader.bytes_read(),
            Self::Csv(_) => 0,
        }
    }
}

/// What the call's named parameters said about how the CSV files are written.
///
/// The binder worked this out to sniff the files with and wrote the names and the values into the
/// plan, and this works it out again from them to read the files with. Both go through
/// [`csv_given`], so a file is read the way it was sniffed and the columns a query was planned
/// against are the columns it reads. A `read_parquet` call has none of these and gets the default,
/// which says nothing and is never asked.
fn csv_options(plan: &Plan, options: Slice, settings: Slice) -> Result<Given> {
    if options.len == 0 {
        return Ok(Given::default());
    }
    let exprs: Vec<ExprRef> = plan.expr_list(settings).to_vec();
    let source = Schema::empty();
    let one = Chunk::with_rows(Vec::new(), 1)?;
    let evaluated = evaluate_all(plan, &exprs, &source, &one)?;
    let names: Vec<&str> = plan.name_list(options).iter().map(|name| plan.string(*name)).collect();
    let written: Vec<(&str, rudb_common::Value)> =
        names.into_iter().zip(evaluated.iter().map(|vector| vector.value_at(0))).collect();
    csv_given(&written)
}

/// The file names a file reading table function was called with.
///
/// The binder already refused anything that is not a constant string and already expanded whatever
/// patterns there were, so a failure here is a plan that was built wrong rather than a statement
/// somebody wrote wrong, and it says so.
pub(crate) fn file_arguments(
    plan: &Plan,
    args: Slice,
    function: TableFunction,
) -> Result<Vec<String>> {
    let exprs: Vec<ExprRef> = plan.expr_list(args).to_vec();
    let source = Schema::empty();
    let one = Chunk::with_rows(Vec::new(), 1)?;
    let evaluated = evaluate_all(plan, &exprs, &source, &one)?;
    let mut paths = Vec::with_capacity(evaluated.len());
    for vector in &evaluated {
        match vector.value_at(0) {
            rudb_common::Value::Varchar(path) => paths.push(path),
            other => {
                return Err(Error::internal(format!(
                    "{}() bound with {other:?} rather than constant file names",
                    function.name()
                )));
            }
        }
    }
    Ok(paths)
}

/// Where in `held` each of `wanted` is, by name.
///
/// `first` is the file the schema came from, and is `None` when `path` is that file. The two say
/// different things about a missing column and DuckDB writes both: the first file is the one the
/// plan was bound against, so a column missing from it means the file was replaced since, while a
/// column missing from a later one means the files in the set do not agree with each other.
///
/// The disagreement is worded by whichever reader found it, because the two readers in DuckDB are
/// two pieces of code that each wrote their own sentence and a compatibility test that compares
/// output compares all of it. For CSV this is only reachable when a file changed between binding and
/// running, since the binder sniffed every file and would have said the same thing first.
pub(crate) fn positions(
    function: TableFunction,
    wanted: &[Field],
    held: &[Field],
    path: &str,
    first: Option<&str>,
) -> Result<Vec<usize>> {
    let mut positions = Vec::with_capacity(wanted.len());
    for field in wanted {
        let at = held.iter().position(|column| column.name == field.name).ok_or_else(|| {
            let Some(first) = first else {
                return Error::io(format!(
                    "File \"{path}\" does not have a column named \"{}\"",
                    field.name
                ));
            };
            if matches!(function, TableFunction::ReadCsv) {
                return rudb_csv::mismatch(first, path, &field.name);
            }
            let candidates: Vec<&str> = held.iter().map(|column| column.name.as_str()).collect();
            Error::invalid_input(format!(
                "Failed to read file \"{path}\": schema mismatch in glob: column \"{}\" was read \
                 from the original file \"{first}\", but could not be found in file \
                 \"{path}\".\nCandidate names: {}\nIf you are trying to read files with different \
                 schemas, try setting union_by_name=True",
                field.name,
                candidates.join(", ")
            ))
        })?;
        positions.push(at);
    }
    Ok(positions)
}

impl Source for Values {
    fn morsel(&self) -> Option<Morsel> {
        self.handout.take()
    }

    fn morsels(&self) -> Option<usize> {
        Some(self.handout.total())
    }

    fn read(&self, morsel: &mut Morsel, out: &mut Chunk) -> Result<Progress> {
        *out = match self.chunks.get(position(morsel)) {
            Some(chunk) => chunk.clone(),
            None => Chunk::empty(&self.schema.types()),
        };
        morsel.advance(1);
        Ok(Progress::Done)
    }
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};

    use rudb_catalog::{QualifiedName, Table};
    use rudb_common::{Field, LogicalType, Value};
    use rudb_functions::TableFunction;
    use rudb_pipeline::{Progress, Source};
    use rudb_plan::{Node, Plan};
    use rudb_vector::Chunk;

    use super::{
        Bound, FileScan, Handout, Op, Probe, RUN, Scan, Schema, Series, VECTOR_SIZE, next_piece,
        parts,
    };

    /// Every morsel a row group of `rows` rows is cut into, by asking for them the way `cut` does.
    fn cutting(rows: usize, target: usize) -> Vec<std::ops::Range<usize>> {
        let mut out = Vec::new();
        let mut part = 0;
        loop {
            let piece = next_piece(rows, part, target);
            if piece.is_empty() {
                return out;
            }
            part = piece.end;
            out.push(piece);
            assert!(out.len() <= rows + 1, "cutting {rows} rows into {target} did not terminate");
        }
    }

    /// A series without going through a plan, which is what `Series::new` is for.
    fn series(start: i64, step: i64, rows: u64) -> Series {
        Series { schema: Schema::empty(), start, step, rows, morsels: AtomicU64::new(0) }
    }

    /// Every value the series produces, and how many morsels it took to produce them.
    fn drained(series: &Series) -> (Vec<i64>, usize) {
        let mut values = Vec::new();
        let mut morsels = 0;
        while let Some(mut morsel) = series.morsel() {
            morsels += 1;
            loop {
                let mut chunk = Chunk::empty(&[LogicalType::BigInt]);
                let progress = series.read(&mut morsel, &mut chunk).expect("a series reads");
                for row in 0..chunk.len() {
                    match chunk.value_at(row, 0) {
                        Value::BigInt(value) => values.push(value),
                        other => panic!("a series produced {other}"),
                    }
                }
                if progress == Progress::Done {
                    break;
                }
            }
        }
        (values, morsels)
    }

    /// A morsel holds more than a chunk, so reading one is several calls, and the last of them is
    /// the one that says the morsel is done.
    #[test]
    fn a_morsel_of_a_series_is_read_a_chunk_at_a_time() {
        let rows = VECTOR_SIZE as u64 * 2 + 5;
        let (values, morsels) = drained(&series(0, 1, rows));

        assert_eq!(morsels, 1);
        assert_eq!(values.len(), rows as usize);
        assert_eq!(values[0], 0);
        assert_eq!(values[values.len() - 1], rows as i64 - 1);
    }

    /// The value at a position is worked out from the position rather than counted up to, because
    /// the thread that gets the second morsel never saw the first one.
    #[test]
    fn a_series_longer_than_a_morsel_carries_on_where_the_last_one_stopped() {
        let rows = RUN + 3;
        let (values, morsels) = drained(&series(10, 3, rows));

        assert_eq!(morsels, 2);
        assert_eq!(values.len(), rows as usize);
        assert_eq!(values[0], 10);
        assert_eq!(values[RUN as usize], 10 + 3 * RUN as i64);
        assert_eq!(values[values.len() - 1], 10 + 3 * (rows as i64 - 1));
    }

    /// Nothing to produce is no morsels at all, rather than one morsel that produces nothing, which
    /// is what a null argument to `range()` means.
    #[test]
    fn a_series_of_nothing_hands_out_no_work() {
        let (values, morsels) = drained(&series(0, 1, 0));

        assert!(values.is_empty());
        assert_eq!(morsels, 0);
    }

    /// The whole of what makes a source shareable: a position goes to one caller and the next
    /// caller gets the next one, so two threads scanning a table read different chunks of it.
    #[test]
    fn a_handout_gives_each_position_to_one_caller_and_then_stops() {
        let handout = Handout::new(3);

        let taken: Vec<u64> = (0..3).map(|_| handout.take().expect("a position").start()).collect();

        assert_eq!(taken, [0, 1, 2]);
        assert!(handout.take().is_none());
        assert!(handout.take().is_none());
    }

    /// A scan of the `rudb-parquet` fixture, which is 4096 rows in two row groups.
    ///
    /// Built from a plan written as text, the way every other operator test in this crate builds
    /// one, because the fields a table function node carries are arena slices and writing them out
    /// by hand would be a test of the arena builders.
    fn fixture() -> FileScan {
        pruned(Vec::new())
    }

    /// The same scan with bounds tests on it, which is what a filter above the scan compiles to.
    fn pruned(tests: Vec<(usize, Op, Bound)>) -> FileScan {
        let path = format!("{}/../rudb-parquet/testdata/mixed.parquet", env!("CARGO_MANIFEST_DIR"));
        let text = format!(
            "TableFunction read_parquet args=['{path}'::VARCHAR] #0 [a::INTEGER, b::BIGINT]"
        );
        let plan = Plan::parse(&text).expect("the plan text round trips");
        let Node::TableFunction { index, args, options, settings, columns, .. } =
            *plan.node(plan.root())
        else {
            panic!("the plan is a table function");
        };
        FileScan::new(
            &plan,
            index,
            TableFunction::ReadParquet,
            args,
            options,
            settings,
            columns,
            tests,
        )
        .expect("the fixture is there")
    }

    /// Every morsel the scan hands out, drained.
    fn morsels(scan: &FileScan) -> Vec<usize> {
        let mut rows = Vec::new();
        while let Some(mut morsel) = scan.morsel() {
            let mut chunk = Chunk::empty(&[]);
            let mut read = 0;
            while let Progress::More = scan.read(&mut morsel, &mut chunk).expect("decodes") {
                read += chunk.len();
            }
            rows.push(read);
        }
        rows
    }

    /// The property the scheduler needs. A file of two row groups is two units of work, not one,
    /// and the two between them hold every row of the file.
    #[test]
    fn a_parquet_file_is_one_morsel_per_row_group() {
        let scan = fixture();
        let rows = morsels(&scan);
        assert_eq!(rows, [2048, 2048], "two row groups of 2048");
    }

    /// The point of the bounds tests. The integer column of the fixture runs 0 to 96, so a filter
    /// asking for rows above a thousand cannot be satisfied by either row group, and neither group
    /// is handed out at all. No morsel means no page was read, which is the whole saving.
    #[test]
    fn a_row_group_whose_bounds_rule_out_the_filter_is_never_handed_out() {
        let scan = pruned(vec![(0, Op::Greater, Bound::Int(1_000))]);

        let rows = morsels(&scan);

        assert_eq!(rows, Vec::<usize>::new(), "both row groups are ruled out");
        let cutting = scan.cutting.lock().expect("the lock holds");
        assert_eq!(cutting.skipped, 2, "and both were skipped rather than read");
    }

    /// The other half of it. A bound that overlaps the column leaves the scan exactly as it was,
    /// because a row group the statistics cannot rule out has to be read and filtered as usual.
    #[test]
    fn a_row_group_whose_bounds_overlap_the_filter_is_handed_out_as_usual() {
        let scan = pruned(vec![(0, Op::Greater, Bound::Int(50))]);

        let rows = morsels(&scan);

        assert_eq!(rows, [2048, 2048], "nothing is ruled out");
        let cutting = scan.cutting.lock().expect("the lock holds");
        assert_eq!(cutting.skipped, 0);
    }

    /// A test naming a column the scan does not produce is dropped rather than misread as column
    /// zero, which would skip row groups holding rows the query wants.
    #[test]
    fn a_test_against_a_position_the_scan_does_not_produce_rules_nothing_out() {
        let scan = pruned(vec![(7, Op::Greater, Bound::Int(1_000))]);

        assert_eq!(morsels(&scan), [2048, 2048]);
    }

    /// Morsels are taken until they run out, and asking after that hands back nothing rather than
    /// starting again, which is what makes a driver loop safe to write as a `while let`.
    #[test]
    fn a_scan_that_has_handed_out_every_morsel_hands_out_no_more() {
        let scan = fixture();
        let _ = morsels(&scan);
        assert!(scan.morsel().is_none());
        assert!(scan.morsel().is_none());
    }

    /// Each morsel carries its own reader, so taking them all before reading any of them reads the
    /// same rows as taking and reading them one at a time. That is the difference between a scan
    /// two threads can share and a scan they queue behind.
    #[test]
    fn every_morsel_can_be_taken_before_any_of_them_is_read() {
        let scan = fixture();
        let mut taken = Vec::new();
        while let Some(morsel) = scan.morsel() {
            taken.push(morsel);
        }
        assert_eq!(taken.len(), 2);
        let mut rows = Vec::new();
        for morsel in &mut taken {
            let mut chunk = Chunk::empty(&[]);
            let mut read = 0;
            while let Progress::More = scan.read(morsel, &mut chunk).expect("decodes") {
                read += chunk.len();
            }
            rows.push(read);
        }
        assert_eq!(rows, [2048, 2048]);
    }

    /// A table of one `INTEGER` column holding `0..rows`, which puts a different range in every
    /// chunk and so makes the zone maps worth having.
    fn counted(rows: usize) -> Table {
        let mut table = Table::new(
            QualifiedName::new("memory", "main", "t"),
            vec![Field::new("n", LogicalType::Integer)],
        )
        .expect("one column");
        let values: Vec<Vec<Value>> = (0..rows).map(|n| vec![Value::Integer(n as i32)]).collect();
        table.append_rows(&values).expect("integers");
        table
    }

    /// A scan of that table with `tests` on its only column, built without going through a plan.
    fn scanning(table: &Table, tests: Vec<(usize, Op, Bound)>) -> Scan<'_> {
        let fields = vec![Field::new("n", LogicalType::Integer)];
        let probes =
            tests.into_iter().map(|(column, op, value)| Probe { column, op, value }).collect();
        Scan {
            table,
            columns: vec![0],
            probes,
            schema: Schema::numbered(fields, 0),
            chunks: Handout::new(table.rows().chunk_count()),
            skipped: AtomicUsize::new(0),
        }
    }

    /// How many rows the scan produced, over every morsel it hands out.
    fn counted_rows(scan: &Scan<'_>) -> usize {
        let mut rows = 0;
        while let Some(mut morsel) = scan.morsel() {
            let mut chunk = Chunk::empty(&[LogicalType::Integer]);
            loop {
                let progress = scan.read(&mut morsel, &mut chunk).expect("a table scan reads");
                rows += chunk.len();
                if progress == Progress::Done {
                    break;
                }
            }
        }
        rows
    }

    /// The point of the zone maps. Five chunks hold 0 to 10239, and `n = 5000` is in exactly one of
    /// them, so four are never read at all and the filter above never sees their rows.
    #[test]
    fn a_filter_on_a_table_reads_only_the_chunks_that_can_hold_a_match() {
        let table = counted(VECTOR_SIZE * 5);
        assert_eq!(table.rows().chunk_count(), 5);
        let scan = scanning(&table, vec![(0, Op::Equal, Bound::Int(5_000))]);

        assert_eq!(counted_rows(&scan), VECTOR_SIZE, "one chunk's worth");
        assert_eq!(scan.skipped.load(Ordering::Relaxed), 4);
    }

    /// A scan with nothing to go on reads everything, which is the case that must not regress.
    #[test]
    fn a_scan_with_no_tests_reads_every_chunk() {
        let table = counted(VECTOR_SIZE * 3);
        let scan = scanning(&table, Vec::new());

        assert_eq!(counted_rows(&scan), VECTOR_SIZE * 3);
        assert_eq!(scan.skipped.load(Ordering::Relaxed), 0);
    }

    /// Two conjuncts that between them leave no chunk, which is the shape ClickBench 37 has.
    #[test]
    fn conjuncts_that_rule_out_every_chunk_read_nothing() {
        let table = counted(VECTOR_SIZE * 4);
        let scan = scanning(
            &table,
            vec![(0, Op::GreaterOrEqual, Bound::Int(2_048)), (0, Op::Less, Bound::Int(2_048))],
        );

        assert_eq!(counted_rows(&scan), 0);
        assert_eq!(scan.skipped.load(Ordering::Relaxed), 4);
    }

    /// The cutting of a row group, driven directly because every committed fixture is smaller than
    /// one morsel and so cannot be cut into more than one.
    #[test]
    fn the_morsels_of_a_row_group_tile_it_once_each() {
        for rows in [1, 2, 7, 9, 10, 100, 4_095, 4_096, 4_097, 122_880, 123_554] {
            for target in [1, 2, 3, 300, 4_096, 32_768] {
                let pieces = cutting(rows, target);
                assert_eq!(pieces.len(), parts(rows, target), "{rows} rows in morsels of {target}");
                let mut at = 0;
                for piece in &pieces {
                    assert_eq!(piece.start, at, "{rows} rows in morsels of {target}");
                    assert!(piece.len() <= target, "{rows} rows in morsels of {target}");
                    at = piece.end;
                }
                assert_eq!(at, rows, "{rows} rows in morsels of {target}");
            }
        }
    }

    /// Even morsels rather than full ones and a remainder, because the remainder is the morsel every
    /// other thread waits for.
    #[test]
    fn a_row_group_is_cut_into_even_morsels() {
        // A DuckDB row group in four, which is 30889 rows three times and 30887 once rather than
        // 32768 three times and 25250 once.
        let pieces = cutting(123_554, 32_768);
        assert_eq!(pieces.len(), 4);
        let longest = pieces.iter().map(std::ops::Range::len).max().expect("four morsels");
        let shortest = pieces.iter().map(std::ops::Range::len).min().expect("four morsels");
        assert_eq!(longest, 30_889);
        assert_eq!(shortest, 30_887);
        assert!(longest - shortest < pieces.len(), "morsels of {longest} and {shortest} rows");
    }

    /// The common case, which is every row group of every file small enough not to be worth cutting.
    #[test]
    fn a_row_group_no_larger_than_a_morsel_is_one_morsel() {
        assert_eq!(cutting(2_048, 32_768), vec![0..2_048]);
        assert_eq!(cutting(32_768, 32_768), vec![0..32_768]);
        assert_eq!(parts(2_048, 32_768), 1);
    }

    /// What stops a file with an empty row group in it from being cut forever. The group is counted
    /// as one morsel by `parts` and handed out as none, which is the safe way round because the
    /// count is only ever used to decide how many copies of a pipeline to build.
    #[test]
    fn an_empty_row_group_is_no_morsels() {
        assert!(next_piece(0, 0, 32_768).is_empty());
        assert_eq!(cutting(0, 32_768), Vec::new());
    }
}
