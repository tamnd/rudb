//! Running a lambda's body over the elements of a list.
//!
//! `list_transform(l, lambda x: x + k)` runs its body once per element, and the body is an ordinary
//! expression, so it is run the ordinary way over a chunk of its own. That chunk has a row per
//! element rather than per row of the list. Its columns are the operator's columns, each element
//! getting the values of the row its list is in, and then the parameters: the element, and its
//! position counting from one when the lambda was written with two. The body was bound against
//! exactly that layout, with the parameters under a table index of their own, so it runs over the
//! chunk without knowing it is inside a lambda at all.
//!
//! Only the columns the body reads are carried over. The rest are there as nulls, because a
//! column the body does not read costs a gather per element for nothing, and the body cannot tell
//! the difference. A lambda inside another lambda's body reads the outer one's parameters the way
//! it reads a column, since those are columns of the chunk it runs over, and they are carried the
//! same way.
//!
//! The elements go through in chunks of at most [`VECTOR_SIZE`], which is the size every
//! expression in the engine is built to run over, however long the lists are.
//!
//! `list_reduce` is the one that cannot run every element at once, because each step reads what
//! the step before it made. It runs by position instead: every list's second element in one
//! chunk, then every list's third, with a row per list that still has one, so a chunk of lists
//! is as many runs of the body as its longest list is long, and never one per element.
//!
//! `invoke` has no list at all. Its body runs once over the chunk as it is, with the arguments as
//! the parameters, which makes it the same machinery with a row per row instead of per element.

use rudb_common::{Error, Field, LogicalType, Result, Value};
use rudb_kernels::{cast, is_true};
use rudb_plan::{ColumnBinding, Expr, ExprRef, Plan, Slice};
use rudb_vector::{Buffer, Chunk, Data, VECTOR_SIZE, Validity, Vector};

use crate::schema::Schema;

/// What a function that takes a lambda does with the body's answers.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Kind {
    /// The answers are the new elements.
    Transform,
    /// The answers say which elements stay.
    Filter,
    /// The answer is the next step's accumulator, and the last one is the result.
    Reduce,
    /// The answers are the answer, a row each.
    Invoke,
}

/// The lambda of a call to a function that takes one, and the call's other arguments in order, if
/// this call is one.
///
/// The binder writes these calls with the lambda as the second argument, after the list, or as the
/// first for `invoke`, and it writes a lambda nowhere else. The other arguments are the list and
/// `list_reduce`'s initial value, or `invoke`'s parameters.
pub(crate) fn lambda_call(plan: &Plan, args: Slice) -> Option<(ExprRef, Vec<ExprRef>)> {
    let is_lambda = |lambda: ExprRef| matches!(plan.expr(lambda), Expr::Lambda { .. });
    let args = plan.expr_list(args);
    match args {
        [lambda, rest @ ..] if is_lambda(*lambda) => Some((*lambda, rest.to_vec())),
        [list, lambda, rest @ ..] if is_lambda(*lambda) && rest.len() <= 1 => {
            Some((*lambda, [*list].into_iter().chain(rest.iter().copied()).collect()))
        }
        _ => None,
    }
}

/// Everything about one lambda call that is decided before a chunk arrives.
#[derive(Debug)]
pub(crate) struct Lambda {
    kind: Kind,
    /// What the body runs over: the operator's columns and then the parameters.
    schema: Schema,
    /// Per column of the operator, whether the body reads it.
    captured: Vec<bool>,
    /// How many parameters there are, one to three.
    params: usize,
    /// What the body produces, which is the element type of a transform's answer and the type of
    /// a reduction's accumulator.
    body_type: LogicalType,
}

impl Lambda {
    /// Works out the layout for one call, over an operator whose output is `schema`.
    ///
    /// # Errors
    ///
    /// An internal error if the call is not a function that takes a lambda.
    pub(crate) fn new(
        plan: &Plan,
        name: &str,
        lambda: ExprRef,
        inputs: &[ExprRef],
        schema: &Schema,
    ) -> Result<Self> {
        let kind = match name {
            "list_transform" => Kind::Transform,
            "list_filter" => Kind::Filter,
            "list_reduce" => Kind::Reduce,
            "invoke" => Kind::Invoke,
            other => return Err(Error::internal(format!("{other} does not take a lambda"))),
        };
        let Expr::Lambda { table, params, body } = *plan.expr(lambda) else {
            return Err(Error::internal("a lambda call without a lambda"));
        };
        let names = plan.name_list(params);
        let body_type = plan.expr_type(body).clone();
        let mut types = if kind == Kind::Invoke {
            inputs.iter().map(|&input| plan.expr_type(input).clone()).collect()
        } else {
            let list = inputs.first().copied();
            let Some(LogicalType::List(element)) = list.map(|list| plan.expr_type(list)) else {
                return Err(Error::internal(format!("{name} over something other than a list")));
            };
            match kind {
                Kind::Reduce => vec![body_type.clone(), (**element).clone(), LogicalType::BigInt],
                _ => vec![(**element).clone(), LogicalType::BigInt],
            }
        };
        read_types(plan, body, table, &mut types);
        let mut fields = Vec::with_capacity(names.len());
        let mut bindings = Vec::with_capacity(names.len());
        for (at, (&name, ty)) in names.iter().zip(types).enumerate() {
            fields.push(Field::new(plan.string(name), ty));
            bindings.push(ColumnBinding::new(table, at as u32));
        }
        let mut captured = vec![false; schema.width()];
        plan.read_columns(body, &mut |_, binding| {
            if let Some(position) = schema.position_of(binding) {
                captured[position] = true;
            }
        });
        plan.read_parameters(body, &mut |binding| {
            if let Some(position) = schema.position_of(binding) {
                captured[position] = true;
            }
        });
        Ok(Self {
            kind,
            schema: Schema::concat(schema, &Schema::new(fields, bindings)?),
            captured,
            params: names.len(),
            body_type,
        })
    }

    /// What the body is run over, which is what it has to be prepared against.
    pub(crate) fn schema(&self) -> &Schema {
        &self.schema
    }

    /// Runs the call over one chunk, given the other arguments as long as the chunk and a way to
    /// run the body.
    ///
    /// A null list is a null answer, and an empty one is an empty answer that ran nothing.
    ///
    /// # Errors
    ///
    /// Whatever the body raises, on the first element that raises it.
    pub(crate) fn run(
        &self,
        inputs: &[&Vector],
        chunk: &Chunk,
        body: &mut dyn FnMut(&Chunk) -> Result<Vector>,
    ) -> Result<Vector> {
        let rows = chunk.len();
        if self.kind == Kind::Invoke {
            let mut columns = Vec::with_capacity(self.schema.width());
            for (position, &read) in self.captured.iter().enumerate() {
                columns.push(if read {
                    chunk.column(position)?.clone()
                } else {
                    let ty = self.schema.fields()[position].ty.clone();
                    Vector::constant(ty, Value::Null, rows)
                });
            }
            columns.extend(inputs.iter().map(|&input| input.clone()));
            return body(&Chunk::with_rows(columns, rows)?);
        }
        let (Some(list), initial) = (inputs.first(), inputs.get(1).copied()) else {
            return Err(Error::internal("a lambda over a list without the list"));
        };
        // flatten: `list_parts` reads the entries and the child of a flat list, and a list argument
        // can arrive as a constant or through a dictionary.
        let flat = list.flatten()?;
        let Some((entries, child)) = flat.list_parts() else {
            return Err(Error::internal(format!("a lambda over a {} vector", list.logical_type())));
        };
        if self.kind == Kind::Reduce {
            return self.reduce(&flat, entries, child, initial, chunk, body);
        }
        let mut batch = Batch::default();
        let mut pieces = Vec::new();
        let mut kept: Vec<u32> = Vec::new();
        let mut counts = vec![0u32; rows];
        for (row, &(start, len)) in entries.iter().enumerate().take(rows) {
            if flat.is_null_at(row) {
                continue;
            }
            for offset in 0..len {
                batch.rows.push(row as u32);
                batch.elements.push(start + offset);
                if batch.rows.len() == VECTOR_SIZE {
                    self.flush(
                        &mut batch,
                        chunk,
                        child,
                        body,
                        &mut pieces,
                        &mut kept,
                        &mut counts,
                    )?;
                }
            }
        }
        if !batch.rows.is_empty() {
            self.flush(&mut batch, chunk, child, body, &mut pieces, &mut kept, &mut counts)?;
        }
        let elements = match self.kind {
            Kind::Transform => joined(&self.body_type, pieces)?,
            Kind::Filter | Kind::Reduce | Kind::Invoke => child.gather(&kept)?,
        };
        let mut entries = Vec::with_capacity(rows);
        let mut at = 0u32;
        for &count in &counts {
            entries.push((at, count));
            at += count;
        }
        let answer = Vector::list(entries, elements)?;
        if (0..rows).any(|row| flat.is_null_at(row)) {
            return Ok(answer.with_validity(Validity::from_iter(rows, |row| !flat.is_null_at(row))));
        }
        Ok(answer)
    }

    /// Runs the body over the elements collected so far and empties the batch.
    #[allow(clippy::too_many_arguments)]
    fn flush(
        &self,
        batch: &mut Batch,
        chunk: &Chunk,
        child: &Vector,
        body: &mut dyn FnMut(&Chunk) -> Result<Vector>,
        pieces: &mut Vec<Vector>,
        kept: &mut Vec<u32>,
        counts: &mut [u32],
    ) -> Result<()> {
        let len = batch.rows.len();
        let mut columns = self.carried(chunk, &batch.rows)?;
        columns.push(child.gather(&batch.elements)?);
        if self.params == 2 {
            let mut positions = Vec::with_capacity(len);
            let mut previous = None;
            let mut index = 0i64;
            for &row in &batch.rows {
                index = if previous == Some(row) { index + 1 } else { batch.first_index(row) };
                previous = Some(row);
                positions.push(index);
            }
            batch.last_index = Some((previous.unwrap_or_default(), index));
            columns
                .push(Vector::flat(LogicalType::BigInt, Data::Int64(Buffer::from_vec(positions)))?);
        }
        let inner = Chunk::with_rows(columns, len)?;
        let answers = body(&inner)?;
        match self.kind {
            Kind::Transform => {
                for &row in &batch.rows {
                    counts[row as usize] += 1;
                }
                pieces.push(answers);
            }
            Kind::Reduce | Kind::Invoke => {
                return Err(Error::internal("a lambda without elements run a batch at a time"));
            }
            Kind::Filter => {
                // row at a time: one boolean answer per element of the batch, which is at most one
                // vector's worth, and each one decides whether that element is kept.
                for (at, (&row, &element)) in batch.rows.iter().zip(&batch.elements).enumerate() {
                    if is_true(&answers.value_at(at)) {
                        counts[row as usize] += 1;
                        kept.push(element);
                    }
                }
            }
        }
        batch.rows.clear();
        batch.elements.clear();
        Ok(())
    }
}

impl Lambda {
    /// The operator's columns for the rows in `rows`, one per element, with the ones the body does
    /// not read left as nulls.
    fn carried(&self, chunk: &Chunk, rows: &[u32]) -> Result<Vec<Vector>> {
        let mut columns = Vec::with_capacity(self.schema.width());
        for (position, &read) in self.captured.iter().enumerate() {
            columns.push(if read {
                chunk.column(position)?.gather(rows)?
            } else {
                let ty = self.schema.fields()[position].ty.clone();
                Vector::constant(ty, Value::Null, rows.len())
            });
        }
        Ok(columns)
    }

    /// Folds every list, a position at a time.
    ///
    /// The accumulator starts as the initial value, or as the first element when there is none,
    /// in which case the first step is the second element and its position is `2`. A list that
    /// has run out keeps the accumulator it had as its answer, and a null list is a null answer. An
    /// empty list with nothing to start from is the pin's refusal, and it is refused when a chunk
    /// holding one arrives, not when the query is bound.
    fn reduce(
        &self,
        flat: &Vector,
        entries: &[(u32, u32)],
        child: &Vector,
        initial: Option<&Vector>,
        chunk: &Chunk,
        body: &mut dyn FnMut(&Chunk) -> Result<Vector>,
    ) -> Result<Vector> {
        let rows = chunk.len();
        let mut answers = vec![Value::Null; rows];
        let mut active: Vec<u32> = Vec::with_capacity(rows);
        for (row, &(_, len)) in entries.iter().enumerate().take(rows) {
            if flat.is_null_at(row) {
                continue;
            }
            if len == 0 && initial.is_none() {
                return Err(Error::parameter_not_allowed(
                    "Cannot perform list_reduce on an empty input list",
                ));
            }
            active.push(row as u32);
        }
        let (mut accumulator, mut step) = match initial {
            Some(initial) => (initial.gather(&active)?, 0u32),
            None => {
                let firsts: Vec<u32> = active.iter().map(|&row| entries[row as usize].0).collect();
                (cast(&child.gather(&firsts)?, &self.body_type, false)?, 1)
            }
        };
        // The accumulator's parameter is typed by the body's first binding and what the body makes
        // is typed by its last, and the two differ when the pin's rebinding widened a decimal.
        let carried_as = self.schema.fields()[self.captured.len()].ty.clone();
        loop {
            let mut kept = Vec::with_capacity(active.len());
            let mut still = Vec::with_capacity(active.len());
            for (at, &row) in active.iter().enumerate() {
                if entries[row as usize].1 > step {
                    kept.push(at as u32);
                    still.push(row);
                } else {
                    answers[row as usize] = accumulator.try_value_at(at)?;
                }
            }
            if still.is_empty() {
                break;
            }
            if still.len() != active.len() {
                accumulator = accumulator.gather(&kept)?;
            }
            active = still;
            let len = active.len();
            let mut columns = self.carried(chunk, &active)?;
            columns.push(if accumulator.logical_type() == &carried_as {
                accumulator
            } else {
                cast(&accumulator, &carried_as, false)?
            });
            if self.params >= 2 {
                let elements: Vec<u32> =
                    active.iter().map(|&row| entries[row as usize].0 + step).collect();
                columns.push(child.gather(&elements)?);
            }
            if self.params == 3 {
                let position = Value::BigInt(i64::from(step) + 1);
                columns.push(Vector::constant(LogicalType::BigInt, position, len));
            }
            accumulator = body(&Chunk::with_rows(columns, len)?)?;
            step += 1;
        }
        Vector::from_values(self.body_type.clone(), &answers)
    }
}

/// Sets each parameter's type to the type its reads in the body have.
///
/// A parameter's type is whatever the binder bound it as, and the binder does not always bind it as
/// the type of what is handed to it: `list_reduce` binds its body twice and keeps the first binding's
/// accumulator type. The reads say which, and a parameter nobody reads keeps the default it came in
/// with, since then its type is never looked at.
fn read_types(plan: &Plan, expr: ExprRef, table: u32, types: &mut [LogicalType]) {
    if let Expr::LambdaParam(binding) = *plan.expr(expr) {
        if binding.table == table {
            if let Some(slot) = types.get_mut(binding.column as usize) {
                *slot = plan.expr_type(expr).clone();
            }
        }
    }
    plan.for_each_operand(expr, &mut |operand| read_types(plan, operand, table, types));
}

/// The elements waiting for the body, as the row each came from and where it is in the list's child.
#[derive(Debug, Default)]
struct Batch {
    rows: Vec<u32>,
    elements: Vec<u32>,
    /// The row the last batch ended in and the position its last element had, so a list that is
    /// split across two batches keeps counting where it left off.
    last_index: Option<(u32, i64)>,
}

impl Batch {
    /// The position of the first element of `row` in this batch.
    fn first_index(&self, row: u32) -> i64 {
        match self.last_index {
            Some((last, index)) if last == row => index + 1,
            _ => 1,
        }
    }
}

/// The body's answers from every batch, end to end.
fn joined(ty: &LogicalType, pieces: Vec<Vector>) -> Result<Vector> {
    match pieces.len() {
        0 => Vector::from_values(ty.clone(), &[]),
        1 => match pieces.into_iter().next() {
            // flatten: the answers become the child of the list being built, and one batch gives
            // back the same flat form the concatenation below gives several, so the child does not
            // depend on how many batches the elements took.
            Some(one) => one.flatten(),
            None => Vector::from_values(ty.clone(), &[]),
        },
        _ => {
            if let Some(joined) = rudb_vector::concat(ty, &pieces)? {
                return Ok(joined);
            }
            let mut values = Vec::new();
            for piece in &pieces {
                for at in 0..piece.len() {
                    values.push(piece.try_value_at(at)?);
                }
            }
            Vector::from_values(ty.clone(), &values)
        }
    }
}
