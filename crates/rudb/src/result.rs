//! What a query hands back.

use std::sync::Arc;

use rudb_arrow::{DataType, Field, RecordBatch, Schema};
use rudb_common::{LogicalType, Memory, Origin, Reservation, Result, Session, Value};
use rudb_metrics::Document;
use rudb_vector::{Chunk, Vector};

/// The rows a query produced, with the names and types of its columns.
///
/// Materialized rather than streamed. A streaming result would have to hold the operator tree,
/// which holds a borrow of the plan and of the catalog, and that would make a result set a value
/// nobody can put in a struct. Section 7.5's streaming result is the M1 answer and it arrives as a
/// second type beside this one rather than as a change to it, because the overwhelming majority of
/// queries somebody embeds a database to run produce a result that fits in memory and the API for
/// those should not be the harder one.
///
/// The chunks are kept as chunks rather than flattened into rows. Anything that wants the columnar
/// form gets it without a transpose, which is what an Arrow export and a dataframe binding both
/// want, and anything that wants a row gets it through [`QueryResult::row`].
#[derive(Debug, Clone)]
pub struct QueryResult {
    /// Everything nothing writes to once the result is made, shared between clones. The one-row
    /// count a trickled `INSERT` answers with is a clone of one held result, and with each list and
    /// the session held apart a clone and its drop were a count up and down for every one of them.
    body: Arc<Body>,
    rows: usize,
    /// What the execution that produced these rows measured about itself, when it was an execution
    /// at all.
    ///
    /// Boxed because a document is most of a kilobyte and a result is moved about by value. Held
    /// inline it made every result that large, and a trickled `INSERT` copied its count answer
    /// four times on the way out.
    metrics: Option<Box<Document>>,
    /// How many rows the statement wrote, when this is the count a writing statement answers with
    /// rather than rows a query produced.
    changes: Option<usize>,
    /// The table column that each column reads with no change, where there is one, or empty when
    /// nothing recorded them.
    origins: Vec<Option<Origin>>,
}

/// The part of a [`QueryResult`] its clones share.
#[derive(Debug)]
struct Body {
    names: Vec<String>,
    types: Vec<LogicalType>,
    chunks: Vec<Chunk>,
    /// Where each chunk starts, so a row number finds its chunk by a search rather than by walking.
    /// The last entry is the row count, which is what makes the search a plain partition point.
    starts: Vec<usize>,
    /// What the chunks are charged against the database's memory limit, given back when the last
    /// handle on the result is dropped.
    ///
    /// Shared because a result is cloneable and a reservation is not. Two handles on one result are
    /// charged once. That under counts, and it is the direction to under count in: a program that
    /// clones a result to hand it to another thread has not doubled its data in any sense it would
    /// recognize, and refusing its next query because it did would be a worse answer than the one
    /// it gets.
    held: Reservation,
    /// The session whose zone decides how zoned values are rendered.
    session: Session,
    /// What the statement has to tell the client that is not an error, in the order it said it.
    notices: Vec<Notice>,
}

/// A message a statement gives that is not an error, such as the one for a `DROP TABLE IF EXISTS`
/// of a table that is not there. A PostgreSQL client gets it as a `NoticeResponse` with the
/// severity `NOTICE`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Notice {
    /// The SQLSTATE, which is `00000` for most notices.
    pub sqlstate: &'static str,
    /// The text, in the words of PostgreSQL.
    pub message: String,
}

impl Notice {
    /// The notice for a name that a statement did not find and did not need.
    #[must_use]
    pub fn skipped(kind: &str, name: &str) -> Self {
        Self { sqlstate: "00000", message: format!("{kind} \"{name}\" does not exist, skipping") }
    }

    /// The notice for a name that a statement found and did not make again.
    #[must_use]
    pub fn exists(sqlstate: &'static str, kind: &str, name: &str) -> Self {
        Self { sqlstate, message: format!("{kind} \"{name}\" already exists, skipping") }
    }
}

impl QueryResult {
    /// A result of the given columns and chunks.
    #[must_use]
    pub(crate) fn new(
        names: Vec<String>,
        types: Vec<LogicalType>,
        chunks: Vec<Chunk>,
        held: Reservation,
    ) -> Self {
        let mut starts = Vec::with_capacity(chunks.len() + 1);
        let mut rows = 0;
        for chunk in &chunks {
            starts.push(rows);
            rows += chunk.len();
        }
        starts.push(rows);
        Self {
            body: Arc::new(Body {
                names,
                types,
                chunks,
                starts,
                held,
                session: Session::new(),
                notices: Vec::new(),
            }),
            rows,
            metrics: None,
            changes: None,
            origins: Vec::new(),
        }
    }

    /// The same result, carrying the table column of each column.
    #[must_use]
    pub(crate) fn with_origins(mut self, origins: &[Option<Origin>]) -> Self {
        self.origins = origins.to_vec();
        self
    }

    /// The same result, carrying the document the execution that produced it filled in.
    #[must_use]
    pub(crate) fn measured(mut self, metrics: Document) -> Self {
        self.metrics = Some(Box::new(metrics));
        self
    }

    /// Carries the session that produced the result so zoned values keep its rendering.
    #[must_use]
    pub(crate) fn in_session(mut self, session: Session) -> Self {
        // Called on a result straight from `Self::new`, which nothing else holds yet.
        let body = Arc::get_mut(&mut self.body);
        debug_assert!(body.is_some(), "a session given to a result already shared");
        if let Some(body) = body {
            body.session = session;
        }
        self
    }

    /// The same result, carrying the notices of the statement.
    #[must_use]
    pub(crate) fn noting(mut self, notices: Vec<Notice>) -> Self {
        if notices.is_empty() {
            return self;
        }
        // Called on a result made for the statement, which nothing else holds yet.
        let body = Arc::get_mut(&mut self.body);
        debug_assert!(body.is_some(), "notices given to a result already shared");
        if let Some(body) = body {
            body.notices = notices;
        }
        self
    }

    /// What the statement had to tell the client that is not an error.
    #[must_use]
    pub fn notices(&self) -> &[Notice] {
        &self.body.notices
    }

    /// A value rendered under the session that produced this result.
    ///
    /// Only a zoned value reads the session at all. Everything else renders the same under every
    /// session, and asking for the offset anyway is a search through that zone's transition table
    /// for an answer nothing then uses. It used to happen once per value whatever the type was,
    /// which is why `offset_from_utc_datetime` was 5.82 percent of the profile of a query with no
    /// timestamp column in it. Per #1119.
    #[must_use]
    pub fn value_text(&self, value: &Value) -> String {
        let instant = match value {
            Value::TimestampTz(micros) => *micros,
            // A zoned value inside a list, a struct or a map is written in the session zone too,
            // which UTC already is.
            Value::List { .. } | Value::Struct(_) | Value::Map { .. }
                if zoned_inside(value) && !self.body.session.session_time_zone().is_utc() =>
            {
                return self.written_inside(value).to_string();
            }
            // A variant is written as the value it holds, zone and all.
            Value::Variant(held) => {
                return self.value_text(&rudb_common::variant::unwrapped(held));
            }
            other => return other.to_string(),
        };
        value.to_string_at_offset(self.body.session.offset_seconds_at(instant))
    }

    /// A nested value with every zoned value in it replaced by its text in the session zone, which
    /// prints the same as the zoned value would, quotes and all, since an element is quoted by what
    /// its text holds and not by its type.
    fn written_inside(&self, value: &Value) -> Value {
        match value {
            Value::TimestampTz(_) => Value::Varchar(self.value_text(value)),
            Value::List { element, values } => Value::List {
                element: element.clone(),
                values: values.iter().map(|value| self.written_inside(value)).collect(),
            },
            Value::Struct(fields) => Value::Struct(
                fields
                    .iter()
                    .map(|(name, value)| (name.clone(), self.written_inside(value)))
                    .collect(),
            ),
            Value::Map { key, value, entries } => Value::Map {
                key: key.clone(),
                value: value.clone(),
                entries: entries
                    .iter()
                    .map(|(key, value)| (self.written_inside(key), self.written_inside(value)))
                    .collect(),
            },
            other => other.clone(),
        }
    }

    /// One cell rendered under the session that produced this result.
    #[must_use]
    pub fn text_at(&self, row: usize, column: usize) -> String {
        self.value_text(&self.value_at(row, column))
    }

    /// A result of no columns and no rows, which is what a statement that writes hands back.
    ///
    /// Distinct from a query that produced no rows only by its width. DuckDB answers a `CREATE
    /// TABLE` with a `Count` column holding zero, and copying that would mean every caller checking
    /// whether a column is the real answer or the acknowledgement. `RETURNING` is the shape that
    /// makes a writing statement produce rows, and when it lands it produces them here.
    #[must_use]
    pub(crate) fn empty() -> Self {
        Self::new(Vec::new(), Vec::new(), Vec::new(), Memory::unlimited().reservation())
    }

    /// What an `INSERT`, `UPDATE` or `DELETE` answers, which on the pin is one `Count` column of
    /// one row holding how many rows it wrote.
    ///
    /// A column the caller did not ask for, unlike the empty result above, because the corpus
    /// checks the count with a query record and a program asks how many rows an update touched.
    /// [`Self::changes`] is what tells it apart from a query that happened to produce the same
    /// shape.
    pub(crate) fn changed(rows: usize) -> Result<Self> {
        // One row is what every trickled `INSERT` answers, and building the column, the chunk and
        // the session for it was about a twentieth of what the insert cost, so it is built once a
        // thread and copied. A copy shares the chunk, which nothing writes to.
        thread_local! {
            static ONE: std::cell::OnceCell<QueryResult> = const { std::cell::OnceCell::new() };
        }
        if rows == 1 {
            return ONE.with(|one| match one.get() {
                Some(held) => Ok(held.clone()),
                None => {
                    let built = Self::counted(1)?;
                    Ok(one.get_or_init(|| built).clone())
                }
            });
        }
        Self::counted(rows)
    }

    /// A result of text columns, for an answer that a front end makes without a query, such as
    /// the rows of `SHOW` in the PostgreSQL server.
    ///
    /// # Errors
    ///
    /// A row with a number of values that is not the number of names.
    pub fn text(names: Vec<String>, rows: &[Vec<String>]) -> Result<Self> {
        let mut chunks = Vec::new();
        for part in rows.chunks(rudb_vector::VECTOR_SIZE) {
            let columns = (0..names.len())
                .map(|column| {
                    let values: Vec<Value> = part
                        .iter()
                        .map(|row| row.get(column).cloned().map_or(Value::Null, Value::Varchar))
                        .collect();
                    Vector::from_values(LogicalType::Varchar, &values)
                })
                .collect::<Result<Vec<_>>>()?;
            chunks.push(Chunk::new(columns)?);
        }
        let types = vec![LogicalType::Varchar; names.len()];
        Ok(Self::new(names, types, chunks, Memory::unlimited().reservation()))
    }

    /// The count result [`Self::changed`] hands out, built afresh.
    fn counted(rows: usize) -> Result<Self> {
        let count = i64::try_from(rows).unwrap_or(i64::MAX);
        let vector = Vector::from_values(LogicalType::BigInt, &[Value::BigInt(count)])?;
        let chunk = Chunk::new(vec![vector])?;
        let mut result = Self::new(
            vec!["Count".to_owned()],
            vec![LogicalType::BigInt],
            vec![chunk],
            Memory::unlimited().reservation(),
        );
        result.changes = Some(rows);
        Ok(result)
    }

    /// How many rows the statement wrote, when it was an `INSERT`, `UPDATE` or `DELETE`, and
    /// `None` when the rows are the answer to a query.
    #[must_use]
    pub fn changes(&self) -> Option<usize> {
        self.changes
    }

    /// The column names, in order.
    #[must_use]
    pub fn names(&self) -> &[String] {
        &self.body.names
    }

    /// The column types, in order.
    #[must_use]
    pub fn types(&self) -> &[LogicalType] {
        &self.body.types
    }

    /// The table column that the column at `column` reads with no change, where there is one.
    #[must_use]
    pub fn origin(&self, column: usize) -> Option<Origin> {
        self.origins.get(column).copied().flatten()
    }

    /// How many bytes this result is charged against the database's memory limit.
    ///
    /// The chunks, not the names and the types, and it is what
    /// [`rudb_common::Memory::used`] stops counting when this is dropped. Worth reading for a
    /// program deciding whether to keep a result or re-run the query for it.
    #[must_use]
    pub fn footprint(&self) -> u64 {
        self.body.held.bytes()
    }

    /// What the execution measured about itself, for a result that came from one.
    ///
    /// There is nothing here for a statement that ran no plan, which is a `SET`, a `CREATE` with no
    /// query in it, or an `EXPLAIN`, since none of those execute anything to measure. Everything
    /// else carries a document with a row per operator and a row per pipeline, which is what
    /// `EXPLAIN ANALYZE` prints and what `--metrics` writes out as JSON.
    ///
    /// It hangs off the result rather than off the connection because a result outlives the query
    /// and two of them can be held at once. Numbers kept on the connection would be the numbers of
    /// whichever query ran most recently, which is not a question anybody is asking when they are
    /// holding the result of a particular one.
    #[must_use]
    pub fn metrics(&self) -> Option<&Document> {
        self.metrics.as_deref()
    }

    /// How many columns.
    #[must_use]
    pub fn width(&self) -> usize {
        self.body.names.len()
    }

    /// How many rows, across every chunk.
    #[must_use]
    pub fn len(&self) -> usize {
        self.rows
    }

    /// Whether the query produced no rows.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.rows == 0
    }

    /// The name of one column, or the empty string if there is no such column.
    #[must_use]
    pub fn column_name(&self, column: usize) -> &str {
        self.body.names.get(column).map_or("", String::as_str)
    }

    /// The type of one column, or `NULL` if there is no such column.
    #[must_use]
    pub fn column_type(&self, column: usize) -> LogicalType {
        self.body.types.get(column).cloned().unwrap_or(LogicalType::Null)
    }

    /// The batches, for a caller that wants the columnar form.
    #[must_use]
    pub fn chunks(&self) -> &[Chunk] {
        &self.body.chunks
    }

    /// How many batches the result is in.
    ///
    /// A caller that walks the columnar form walks this rather than the row count, which is the
    /// whole point of having it: a result of ten million rows is a few thousand chunks and reading
    /// it that way never builds a row.
    #[must_use]
    pub fn chunk_count(&self) -> usize {
        self.body.chunks.len()
    }

    /// One batch, or `None` if it is past the end.
    #[must_use]
    pub fn chunk(&self, at: usize) -> Option<&Chunk> {
        self.body.chunks.get(at)
    }

    /// The batches, one at a time.
    ///
    /// The same walk as `chunks().iter()` and the name a caller looks for. What it is not is a
    /// promise about where the rows came from: they are all here already, and a result that does
    /// not materialize is section 7.5's streaming result, which is a second type beside this one.
    pub fn chunk_iter(&self) -> impl ExactSizeIterator<Item = &Chunk> {
        self.body.chunks.iter()
    }

    /// The batches, taken rather than borrowed.
    ///
    /// For a caller that is turning the result into something else, an Arrow record batch or a
    /// table to append to, and would otherwise clone every chunk to do it.
    #[must_use]
    pub fn into_chunks(self) -> Vec<Chunk> {
        match Arc::try_unwrap(self.body) {
            Ok(body) => body.chunks,
            Err(shared) => shared.chunks.clone(),
        }
    }

    /// Which chunk a row is in, and where in it, or `None` if the row is past the end.
    ///
    /// A search rather than a walk. Reading a large result by row number is the ordinary way a
    /// program uses one, and walking the chunk list for each row makes that quadratic in a result
    /// of a few thousand chunks.
    fn locate(&self, row: usize) -> Option<(usize, usize)> {
        if row >= self.rows {
            return None;
        }
        let at = self.body.starts.partition_point(|&start| start <= row) - 1;
        Some((at, row - self.body.starts[at]))
    }

    /// One value, or null if the row or the column is past the end.
    ///
    /// Null for a row that does not exist rather than an option, because every caller that asks for
    /// a value in range would then have to unwrap one, and a query result is read in a loop over
    /// [`QueryResult::len`].
    #[must_use]
    pub fn value_at(&self, row: usize, column: usize) -> Value {
        match self.locate(row) {
            Some((at, offset)) => self.body.chunks[at].value_at(offset, column),
            None => Value::Null,
        }
    }

    /// One row, left to right, or `None` if it is past the end.
    #[must_use]
    pub fn row(&self, row: usize) -> Option<Vec<Value>> {
        let (at, offset) = self.locate(row)?;
        Some(self.body.chunks[at].row(offset).collect())
    }

    /// Every row in order, which is the shape a test and a script both want.
    pub fn rows(&self) -> impl Iterator<Item = Vec<Value>> + '_ {
        self.body
            .chunks
            .iter()
            .flat_map(|chunk| (0..chunk.len()).map(|row| chunk.row(row).collect()))
    }

    /// One column, top to bottom, across every chunk.
    ///
    /// Empty for a column that is not there. This is the read that matches how the rows are held, so
    /// a program summing a column or handing one to a plotting library never transposes anything.
    pub fn column(&self, column: usize) -> impl Iterator<Item = Value> + '_ {
        let width = self.width();
        self.body
            .chunks
            .iter()
            .filter(move |_| column < width)
            .flat_map(move |chunk| (0..chunk.len()).map(move |row| chunk.value_at(row, column)))
    }

    /// The column names and Arrow types, without converting any values.
    ///
    /// Separate from [`QueryResult::to_arrow`] because a result of no rows has no batches and still
    /// has columns, and a consumer that reads the schema off the first batch would have nothing to
    /// read it off. Cheap enough to call on its own: it looks at the types and never at the rows.
    ///
    /// # Errors
    ///
    /// For a column of a type Arrow has no counterpart for here yet.
    pub fn arrow_schema(&self) -> Result<Schema> {
        let mut fields = Vec::with_capacity(self.width());
        for (name, ty) in self.body.names.iter().zip(self.body.types.iter()) {
            fields.push(Field::new(name.clone(), DataType::of(ty)?));
        }
        Ok(Schema::new(fields))
    }

    /// The result as Arrow record batches, one per chunk.
    ///
    /// One per chunk rather than one for the whole result, because the chunks are what the executor
    /// produced and concatenating them would mean copying every value a second time to build a
    /// single large batch that most consumers immediately walk in pieces anyway. A consumer that
    /// does want one batch has every column in hand to build it.
    ///
    /// A result of no rows converts to no batches. The schema is [`QueryResult::arrow_schema`], and
    /// [`RecordBatch::empty`] turns it into the empty batch for a consumer that needs one.
    ///
    /// # Errors
    ///
    /// For a column of a type Arrow has no counterpart for here yet, and for a chunk whose values
    /// are not the layout its type says they are.
    pub fn to_arrow(&self) -> Result<Vec<RecordBatch>> {
        self.body.chunks.iter().map(|chunk| RecordBatch::of(chunk, &self.body.names)).collect()
    }
}

// Every statement hands a result back by value through a few layers, so its size is a copy paid
// per statement. It was 768 bytes with the metrics document held inline.
const _: () = assert!(size_of::<QueryResult>() <= 256, "a result has grown past 256 bytes");

/// Whether a value holds a `TIMESTAMPTZ` anywhere inside it. A `TIMETZ` carries its own offset and
/// is written the same in every session.
fn zoned_inside(value: &Value) -> bool {
    match value {
        Value::TimestampTz(_) => true,
        Value::List { values, .. } => values.iter().any(zoned_inside),
        Value::Struct(fields) => fields.iter().any(|(_, value)| zoned_inside(value)),
        Value::Map { entries, .. } => {
            entries.iter().any(|(key, value)| zoned_inside(key) || zoned_inside(value))
        }
        _ => false,
    }
}
