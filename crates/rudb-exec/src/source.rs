//! The operators that produce rows without an input: the scan, the dummy and the literal rows.

use rudb_catalog::Table;
use rudb_common::{Error, Field, LogicalType, Result};
use rudb_csv::Reader as CsvReader;
use rudb_functions::{TableFunction, files, open_csv, open_parquet, series_length};
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

/// A scan of the Parquet files one argument names.
///
/// The argument is what the binder already required to be one constant string, and the files it
/// names are worked out here rather than being carried from the binder. Binding and running are
/// separated by however long a prepared statement lives, and a plan that held an open descriptor
/// would hold it for all of that. It also means a pattern is matched again at run time, so a
/// prepared statement over `data/*.parquet` sees the file that arrived since it was prepared, which
/// is what DuckDB does.
///
/// Many files are one stream of rows, read in the order [`files`] gives them, and a file is opened
/// only when the one before it has run out. A hundred file glob is one open at a time rather than a
/// hundred descriptors held from the first chunk, and the reads that matter are inside a file
/// anyway.
///
/// The plan's column list is resolved against each file's by name, which is the same thing
/// [`Scan`] does against a catalog table and for the same reason. Today the binder projects every
/// column in order, so the mapping is the identity, and the moment projection pushdown makes the
/// plan's list a subset the reader reads a subset. That is the difference `spec/engine/05-scan.md`
/// section 5.6 describes between reading two columns of ClickBench and reading a hundred and five.
/// It is also what decides whether a file that disagrees about the schema is an error: the check is
/// on the columns the query asked for, so `count(*)` over a glob whose files disagree about a
/// column nobody selected answers rather than failing, which was measured against the binary.
#[derive(Debug)]
pub(crate) struct ParquetScan {
    files: Vec<String>,
    at: usize,
    reader: Reader,
    wanted: Vec<Field>,
    schema: Schema,
}

impl ParquetScan {
    /// The rows of a `read_parquet` call.
    ///
    /// # Errors
    ///
    /// If nothing matches the path any more, if the first file is unreadable, or if it no longer
    /// has a column the plan asked for, which is what a file replaced between binding and running
    /// looks like.
    pub(crate) fn new(plan: &Plan, index: u32, args: Slice, columns: Slice) -> Result<Self> {
        let path = file_argument(plan, args, "read_parquet")?;
        let files = files(&path)?;
        let wanted = plan.field_list(columns).to_vec();
        let mut reader = open_parquet(&files[0])?;
        reader.project(&positions(&wanted, &reader.fields(), &files[0])?)?;
        let schema = Schema::numbered(wanted.clone(), index);
        Ok(Self { files, at: 0, reader, wanted, schema })
    }
}

impl Operator for ParquetScan {
    fn schema(&self) -> &Schema {
        &self.schema
    }

    fn next(&mut self) -> Result<Option<Chunk>> {
        loop {
            if let Some(chunk) = self.reader.next_chunk()? {
                return Ok(Some(conform(chunk, &self.wanted, &self.files[self.at])?));
            }
            self.at += 1;
            let Some(path) = self.files.get(self.at) else { return Ok(None) };
            let mut reader = open_parquet(path)?;
            let held = reader.fields();
            reader.project(&parquet_positions(&self.wanted, &held, path, &self.files[0])?)?;
            self.reader = reader;
        }
    }
}

/// A scan of the CSV files one argument names.
///
/// The same shape as [`ParquetScan`] and for the same reasons, down to resolving the plan's columns
/// against each file's by name and to reading a glob as one stream. What is behind the two is not
/// the same at all: a Parquet file states its schema and stores each column apart, so reading two of
/// a hundred and five is reading two stretches of the file, while a CSV file states nothing and
/// interleaves everything, so every byte is read and parsed whatever the projection is and the
/// projection only saves the conversion and the copy. That is why the two are separate operators
/// rather than one over a trait: the scan that wants to grow row group skipping and the scan that
/// wants to grow a parallel split of the byte range have nothing in the middle worth sharing yet.
///
/// Each file is sniffed on its own, which matters more here than it looks. The header decision is
/// per file, so a directory of files that all carry the same header line gives the rows of all of
/// them and the header of none, which is the whole reason somebody writes the glob.
#[derive(Debug)]
pub(crate) struct CsvScan {
    files: Vec<String>,
    at: usize,
    reader: CsvReader,
    wanted: Vec<Field>,
    schema: Schema,
}

impl CsvScan {
    /// The rows of a `read_csv` call.
    ///
    /// # Errors
    ///
    /// If nothing matches the path any more, if the first file is unreadable, or if it no longer
    /// has a column the plan asked for, which is what a file replaced between binding and running
    /// looks like.
    pub(crate) fn new(plan: &Plan, index: u32, args: Slice, columns: Slice) -> Result<Self> {
        let path = file_argument(plan, args, "read_csv")?;
        let files = files(&path)?;
        let wanted = plan.field_list(columns).to_vec();
        let mut reader = open_csv(&files[0])?;
        reader.project(&positions(&wanted, &reader.fields(), &files[0])?)?;
        settle(&mut reader, &wanted)?;
        let schema = Schema::numbered(wanted.clone(), index);
        Ok(Self { files, at: 0, reader, wanted, schema })
    }
}

impl Operator for CsvScan {
    fn schema(&self) -> &Schema {
        &self.schema
    }

    fn next(&mut self) -> Result<Option<Chunk>> {
        loop {
            if let Some(chunk) = self.reader.next_chunk()? {
                return Ok(Some(chunk));
            }
            self.at += 1;
            let Some(path) = self.files.get(self.at) else { return Ok(None) };
            let mut reader = open_csv(path)?;
            let held = reader.fields();
            let main = &self.files[0];
            reader.project(&find(&self.wanted, &held, |name| {
                rudb_csv::mismatch(main, path, name)
            })?)?;
            settle(&mut reader, &self.wanted)?;
            self.reader = reader;
        }
    }
}

/// The file name a file reading table function was called with.
///
/// The binder already refused anything that is not one constant string, so a failure here is a plan
/// that was built wrong rather than a statement somebody wrote wrong, and it says so.
fn file_argument(plan: &Plan, args: Slice, function: &str) -> Result<String> {
    let exprs: Vec<ExprRef> = plan.expr_list(args).to_vec();
    let source = Schema::empty();
    let one = Chunk::with_rows(Vec::new(), 1)?;
    let evaluated = evaluate_all(plan, &exprs, &source, &one)?;
    match evaluated.first().map(|vector| vector.value_at(0)) {
        Some(rudb_common::Value::Varchar(path)) => Ok(path),
        other => Err(Error::internal(format!(
            "{function}() bound with {other:?} rather than one constant file name"
        ))),
    }
}

/// Where in `held` each of `wanted` is, by name, with `missing` saying what a column that is not
/// there means.
///
/// The three callers all want the same walk and three different sentences. The first file of a read
/// is a file that changed under a plan bound against it, a later Parquet file is two files
/// disagreeing, and a later CSV file is the same thing worded differently because the two readers in
/// DuckDB are two pieces of code that each wrote their own message.
fn find(wanted: &[Field], held: &[Field], missing: impl Fn(&str) -> Error) -> Result<Vec<usize>> {
    let mut positions = Vec::with_capacity(wanted.len());
    for field in wanted {
        let at = held
            .iter()
            .position(|column| column.name == field.name)
            .ok_or_else(|| missing(&field.name))?;
        positions.push(at);
    }
    Ok(positions)
}

/// A chunk cast to the types the read settled on, which is what a Parquet file after the first
/// needs when it stores a column as something else.
///
/// Measured: a glob whose first file has `a` as INTEGER and whose second has it as DOUBLE answers
/// INTEGER for both rows, so the later file is cast rather than the stream being widened. A cast
/// that cannot be done is an error, and DuckDB's wording for it names the file, the column, both
/// types and the two ways out. The inner sentence is whatever the cast itself said, which is where
/// the offending value gets named.
///
/// The common case is the first file, or a file that agrees, and that is a type comparison per
/// column per chunk and no copy at all.
fn conform(chunk: Chunk, wanted: &[Field], path: &str) -> Result<Chunk> {
    if chunk.types().iter().zip(wanted).all(|(have, field)| *have == field.ty) {
        return Ok(chunk);
    }
    let rows = chunk.len();
    let mut columns = Vec::with_capacity(wanted.len());
    for (vector, field) in chunk.into_columns().into_iter().zip(wanted) {
        if vector.logical_type() == &field.ty {
            columns.push(vector);
            continue;
        }
        let from = vector.logical_type().clone();
        let converted = cast(&vector, &field.ty, false).map_err(|error| {
            let (name, to) = (&field.name, &field.ty);
            Error::conversion(format!(
                "Error while reading file \"{path}\": failed to cast column \"{name}\" from type \
                 {from} to {to}: : {}\n\nIn file \"{path}\" the column \"{name}\" has type {from}, \
                 but we are trying to read it as type {to}.\nThis can happen when reading multiple \
                 Parquet files. The schema information is taken from the first Parquet file by \
                 default. Possible solutions:\n* Enable the union_by_name=True option to combine \
                 the schema of all Parquet files \
                 (https://duckdb.org/docs/stable/data/multiple_files/combining_schemas)\n* Use a \
                 COPY statement to automatically derive types from an existing table.",
                error.message()
            ))
        })?;
        columns.push(converted);
    }
    Chunk::with_rows(columns, rows)
}

/// Where in `held` each of `wanted` is, for the first file of a read.
fn positions(wanted: &[Field], held: &[Field], path: &str) -> Result<Vec<usize>> {
    find(wanted, held, |name| {
        Error::io(format!("File \"{path}\" does not have a column named \"{name}\""))
    })
}

/// Tells a freshly opened CSV file the types the whole read settled on.
///
/// Every file a CSV glob names is sniffed on its own and the read's types are all of those answers
/// combined, so no single file's sniff is the answer, including the first one. A directory where one
/// file holds 4.5 in a column the others fill with whole numbers is a DOUBLE read, and without this
/// the files that hold whole numbers would each hand up a BIGINT column into a stream that is
/// DOUBLE. That is where CSV differs from Parquet, where the first file really does settle it and a
/// later file is cast to what it said.
///
/// This is not a cast. The sniffed type is what the parser was going to convert the text with, and
/// changing it before any row is read means the text is converted to the right type once rather than
/// to the wrong one and then again.
fn settle(reader: &mut CsvReader, wanted: &[Field]) -> Result<()> {
    let types: Vec<LogicalType> = wanted.iter().map(|field| field.ty.clone()).collect();
    reader.retype(&types)
}

/// The same, for a Parquet file after the first, where a missing column is the two files
/// disagreeing.
///
/// DuckDB's message names both files, because knowing that a glob of two hundred files has a
/// mismatch in it is useless without knowing which two disagreed, and it ends with the way out,
/// which is `union_by_name`. The wording is the binary's, measured rather than paraphrased, down to
/// the candidate list that tells somebody who renamed a column what it is called now.
fn parquet_positions(
    wanted: &[Field],
    held: &[Field],
    path: &str,
    original: &str,
) -> Result<Vec<usize>> {
    find(wanted, held, |name| {
        let candidates: Vec<&str> = held.iter().map(|column| column.name.as_str()).collect();
        Error::invalid_input(format!(
            "Failed to read file \"{path}\": schema mismatch in glob: column \"{name}\" was read \
             from the original file \"{original}\", but could not be found in file \
             \"{path}\".\nCandidate names: {}\nIf you are trying to read files with different \
             schemas, try setting union_by_name=True",
            candidates.join(", ")
        ))
    })
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
