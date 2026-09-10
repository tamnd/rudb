//! The operators that produce rows without an input: the scan, the dummy and the literal rows.

use rudb_catalog::Table;
use rudb_common::{Error, Field, LogicalType, Result};
use rudb_functions::{TableFunction, series_length};
use rudb_plan::{ExprRef, Plan, Slice};
use rudb_vector::{Chunk, VECTOR_SIZE, Vector};

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
        let mut values = Vec::with_capacity(count);
        for _ in 0..count {
            values.push(rudb_common::Value::BigInt(self.at));
            self.at = self.at.saturating_add(self.step);
        }
        self.left -= count;
        let vector = Vector::from_values(LogicalType::BigInt, &values)?;
        Ok(Some(Chunk::with_rows(vec![vector], count)?))
    }
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
