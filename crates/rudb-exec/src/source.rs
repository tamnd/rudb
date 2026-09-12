//! The operators that produce rows without an input: the scan, the dummy and the literal rows.
//!
//! These are the bottom of every pipeline and they are all [`Source`] implementations, which is a
//! different shape from the operators above them. A source is shared rather than owned: one object
//! answers [`Source::morsel`] for every thread running the pipeline, so the position it is up to
//! lives in an atomic rather than in a field somebody mutates. What a morsel covers is the source's
//! own business, and the four here mean four different things by it, which is why the type carries
//! numbers and not rows.

use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, Ordering};

use rudb_catalog::Table;
use rudb_common::{Error, Field, LogicalType, Result};
use rudb_csv::Reader as CsvReader;
use rudb_functions::{Given, TableFunction, csv_given, open_csv, open_parquet, series_length};
use rudb_kernels::cast;
use rudb_parquet::Reader;
use rudb_pipeline::{Morsel, Progress, Source};
use rudb_plan::{ExprRef, Plan, Slice};
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
#[derive(Debug)]
pub(crate) struct Scan<'a> {
    table: &'a Table,
    columns: Vec<usize>,
    schema: Schema,
    chunks: Handout,
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
        let schema = Schema::numbered(fields, index);
        let chunks = Handout::new(table.rows().chunk_count());
        Ok(Self { table, columns, schema, chunks })
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

    fn read(&self, morsel: &mut Morsel, out: &mut Chunk) -> Result<Progress> {
        let at = position(morsel);
        if at >= self.table.rows().chunk_count() {
            *out = Chunk::empty(&self.schema.types());
            return Ok(Progress::Done);
        }
        *out = self.table.rows().read(at, &self.columns)?;
        morsel.advance(1);
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
/// There is one morsel and it covers the whole list, so a second thread asking for work here gets
/// none and the read stays on one thread. That is not a gap in this operator, it is what the readers
/// are: both of them are a position in a file and a buffer beside it, and neither can be asked for
/// the tenth chunk without having read the nine before it. A file per morsel is the first thing to
/// do about that, and a Parquet row group per morsel is the real answer, and both of them are a
/// change to the reader rather than to this.
#[derive(Debug)]
pub(crate) struct FileScan {
    function: TableFunction,
    paths: Vec<String>,
    given: Given,
    wanted: Vec<Field>,
    schema: Schema,
    reading: Mutex<Reading>,
    one: Handout,
}

/// Which file the scan is on and the reader that is open on it.
#[derive(Debug)]
struct Reading {
    at: usize,
    reader: Option<FileReader>,
}

impl FileScan {
    /// The rows of a `read_parquet` or `read_csv` call, over every file it names.
    ///
    /// # Errors
    ///
    /// If the first file is gone or unreadable since it was bound, or if it no longer has a column
    /// the plan asked for, which is what a file replaced between binding and running looks like.
    pub(crate) fn new(
        plan: &Plan,
        index: u32,
        function: TableFunction,
        args: Slice,
        options: Slice,
        settings: Slice,
        columns: Slice,
    ) -> Result<Self> {
        let paths = file_arguments(plan, args, function)?;
        let given = csv_options(plan, options, settings)?;
        let wanted = plan.field_list(columns).to_vec();
        let scan = Self {
            function,
            paths,
            given,
            wanted: wanted.clone(),
            schema: Schema::numbered(wanted, index),
            reading: Mutex::new(Reading { at: 0, reader: None }),
            one: Handout::new(1),
        };
        // The first file is opened now rather than on the first read, so that a file that has gone
        // missing since binding is reported where a caller is still asking a question about this
        // scan rather than in the middle of a result.
        {
            let mut reading = scan.reading.lock().map_err(poisoned)?;
            scan.advance(&mut reading)?;
        }
        Ok(scan)
    }

    /// What this scan produces, in the types the first file settled on.
    pub(crate) fn schema(&self) -> &Schema {
        &self.schema
    }

    /// Opens the next file and projects it, or leaves the reader empty at the end of the list.
    fn advance(&self, reading: &mut Reading) -> Result<()> {
        reading.reader = None;
        let Some(path) = self.paths.get(reading.at) else { return Ok(()) };
        let mut reader = FileReader::open(self.function, path, self.given)?;
        let first = if reading.at == 0 { None } else { self.paths.first().map(String::as_str) };
        reader.project(&positions(self.function, &self.wanted, &reader.fields(), path, first)?)?;
        reader.settle(&self.wanted)?;
        reading.reader = Some(reader);
        reading.at += 1;
        Ok(())
    }

    /// The chunk with every column in the type the first file gave it.
    ///
    /// Almost always nothing, because almost always every file has the same schema, and the check is
    /// a type comparison per column per chunk rather than per row.
    ///
    /// `file` is how far through the list the scan is, which is one past the file this chunk came
    /// out of, and the only thing it is for is naming that file if a column will not cast.
    fn conform(&self, chunk: Chunk, file: usize) -> Result<Chunk> {
        let rows = chunk.len();
        let mut columns = Vec::with_capacity(self.wanted.len());
        let mut changed = false;
        for (at, field) in self.wanted.iter().enumerate() {
            let column = chunk.column(at)?;
            if column.logical_type() == &field.ty {
                columns.push(column.clone());
                continue;
            }
            changed = true;
            columns.push(cast(column, &field.ty, false).map_err(|error| {
                let path = self.paths.get(file.saturating_sub(1)).map_or("", String::as_str);
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
        if !changed {
            return Ok(chunk);
        }
        Chunk::with_rows(columns, rows)
    }
}

impl Source for FileScan {
    fn morsel(&self) -> Option<Morsel> {
        self.one.take()
    }

    fn read(&self, morsel: &mut Morsel, out: &mut Chunk) -> Result<Progress> {
        let mut reading = self.reading.lock().map_err(poisoned)?;
        loop {
            let file = reading.at;
            let Some(reader) = reading.reader.as_mut() else {
                morsel.advance(1);
                *out = Chunk::empty(&self.schema.types());
                return Ok(Progress::Done);
            };
            if let Some(chunk) = reader.next_chunk()? {
                // A file that is empty gives no chunk rather than an empty one, so this is not the
                // place that skips it. The loop is.
                *out = self.conform(chunk, file)?;
                return Ok(Progress::More);
            }
            self.advance(&mut reading)?;
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
fn file_arguments(plan: &Plan, args: Slice, function: TableFunction) -> Result<Vec<String>> {
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
fn positions(
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
    use std::sync::atomic::AtomicU64;

    use rudb_common::{LogicalType, Value};
    use rudb_pipeline::{Progress, Source};
    use rudb_vector::Chunk;

    use super::{Handout, RUN, Schema, Series, VECTOR_SIZE};

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
}
