//! The operators that produce rows without an input: the scan, the dummy and the literal rows.

use rudb_catalog::Table;
use rudb_common::{Error, Field, LogicalType, Result};
use rudb_csv::Reader as CsvReader;
use rudb_functions::{Given, TableFunction, csv_given, open_csv, open_parquet, series_length};
use rudb_kernels::cast;
use rudb_parquet::Reader;
use rudb_plan::{ExprRef, Plan, Slice};
use rudb_vector::{Chunk, Data, VECTOR_SIZE, Vector};

use crate::expr::evaluate_all;
use crate::operator::Operator;
use crate::schema::Schema;

/// A base table scan.
///
/// `columns` is the position in the stored table of each column the plan asked for, worked out once
/// when the operator is built. The plan's projection is a list of fields and the table's columns are
/// a list of fields, and they are the same list today only because the binder projects every column
/// in order. Resolving by name rather than assuming that is what keeps this operator correct after
/// projection pushdown makes the plan's list a subset, which is the M1 change section 9.2 describes
/// as the difference between 20 GB and 200 MB on ClickBench.
#[derive(Debug)]
pub(crate) struct Scan<'a> {
    table: &'a Table,
    columns: Vec<usize>,
    schema: Schema,
    at: usize,
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
        Ok(Self { table, columns, schema, at: 0 })
    }
}

impl Operator for Scan<'_> {
    fn schema(&self) -> &Schema {
        &self.schema
    }

    fn next(&mut self) -> Result<Option<Chunk>> {
        if self.at >= self.table.rows().chunk_count() {
            return Ok(None);
        }
        let chunk = self.table.rows().read(self.at, &self.columns)?;
        self.at += 1;
        Ok(Some(chunk))
    }
}

/// One row and no columns.
///
/// What `SELECT 1` sits on. It produces a chunk of width zero and length one exactly once, which is
/// the case `Chunk`'s stored row count exists for.
#[derive(Debug)]
pub(crate) struct Dummy {
    schema: Schema,
    done: bool,
}

impl Dummy {
    pub(crate) fn new() -> Self {
        Self { schema: Schema::empty(), done: false }
    }
}

impl Operator for Dummy {
    fn schema(&self) -> &Schema {
        &self.schema
    }

    fn next(&mut self) -> Result<Option<Chunk>> {
        if self.done {
            return Ok(None);
        }
        self.done = true;
        Ok(Some(Chunk::with_rows(Vec::new(), 1)?))
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
    at: usize,
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
        Ok(Self { schema, chunks, at: 0 })
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
#[derive(Debug)]
pub(crate) struct Series {
    schema: Schema,
    at: i64,
    step: i64,
    left: usize,
}

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
        let left = series_length(function, start, stop, step)?;
        Ok(Self { schema, at: start, step, left })
    }

    fn empty(schema: Schema) -> Self {
        Self { schema, at: 0, step: 1, left: 0 }
    }
}

impl Operator for Series {
    fn schema(&self) -> &Schema {
        &self.schema
    }

    fn next(&mut self) -> Result<Option<Chunk>> {
        if self.left == 0 {
            return Ok(None);
        }
        let count = self.left.min(VECTOR_SIZE);
        // The loop is over `i64` rather than over `Value`, and the vector is built out of the run
        // it fills rather than out of a list of tagged values that would have to be read back one
        // at a time to find the run again. `range()` is the source every microbenchmark in
        // `rudb-bench` reads from, so a chunk of it costing a `Value` a row would be measuring the
        // generator instead of what is downstream of it.
        let mut counted = Vec::with_capacity(count);
        for _ in 0..count {
            counted.push(self.at);
            self.at = self.at.saturating_add(self.step);
        }
        self.left -= count;
        let vector = Vector::flat(LogicalType::BigInt, Data::Int64(counted.into()))?;
        Ok(Some(Chunk::with_rows(vec![vector], count)?))
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
#[derive(Debug)]
pub(crate) struct FileScan {
    function: TableFunction,
    paths: Vec<String>,
    given: Given,
    at: usize,
    reader: Option<FileReader>,
    wanted: Vec<Field>,
    schema: Schema,
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
        let mut scan = Self {
            function,
            paths,
            given,
            at: 0,
            reader: None,
            wanted: wanted.clone(),
            schema: Schema::numbered(wanted, index),
        };
        // The first file is opened now rather than on the first call for `next`, so that a file that
        // has gone missing since binding is reported where a caller is still asking a question about
        // this scan rather than in the middle of a result.
        scan.advance()?;
        Ok(scan)
    }

    /// Opens the next file and projects it, or leaves the reader empty at the end of the list.
    fn advance(&mut self) -> Result<()> {
        self.reader = None;
        let Some(path) = self.paths.get(self.at) else { return Ok(()) };
        let mut reader = FileReader::open(self.function, path, self.given)?;
        let first = if self.at == 0 { None } else { self.paths.first().map(String::as_str) };
        reader.project(&positions(self.function, &self.wanted, &reader.fields(), path, first)?)?;
        reader.settle(&self.wanted)?;
        self.reader = Some(reader);
        self.at += 1;
        Ok(())
    }
}

impl Operator for FileScan {
    fn schema(&self) -> &Schema {
        &self.schema
    }

    fn next(&mut self) -> Result<Option<Chunk>> {
        loop {
            let Some(reader) = self.reader.as_mut() else { return Ok(None) };
            if let Some(chunk) = reader.next_chunk()? {
                // A file that is empty gives no chunk rather than an empty one, so this is not the
                // place that skips it. The loop above is.
                return Ok(Some(self.conform(chunk)?));
            }
            self.advance()?;
        }
    }
}

impl FileScan {
    /// The chunk with every column in the type the first file gave it.
    ///
    /// Almost always nothing, because almost always every file has the same schema, and the check is
    /// a type comparison per column per chunk rather than per row.
    fn conform(&self, chunk: Chunk) -> Result<Chunk> {
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
                let path = self.paths.get(self.at.saturating_sub(1)).map_or("", String::as_str);
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

impl Operator for Values {
    fn schema(&self) -> &Schema {
        &self.schema
    }

    fn next(&mut self) -> Result<Option<Chunk>> {
        if self.at >= self.chunks.len() {
            return Ok(None);
        }
        let chunk = self.chunks[self.at].clone();
        self.at += 1;
        Ok(Some(chunk))
    }
}
