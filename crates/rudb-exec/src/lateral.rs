//! A table function called once per row of its input, which is what `LATERAL` means.
//!
//! `FROM t, range(t.n)` is this operator. Every other correlated shape is answered set at a time,
//! by pushing a relation of the distinct outer values down through the correlated side until the
//! correlation stops, and a table function is where that push has nowhere left to go: its arguments
//! are what produce its rows rather than something read over rows that already exist, so there is
//! nothing underneath it for the relation to be crossed into. This is the operator the unnesting
//! rules hand that relation to.
//!
//! It is still not a loop over the outer rows. What arrives here is the distinct values the
//! correlated columns take, so a query whose outer side has a million rows over five hundred
//! distinct keys makes five hundred calls and not a million. That is the same bargain every other
//! rule in the unnesting pass strikes.
//!
//! Only the series family arrives. A reader takes a file name, the binder settles the columns by
//! opening the file, and it refuses a name that is not a constant, so `read_csv` of a correlated
//! column never reaches a plan at all.

use rudb_common::{Cancel, Error, LogicalType, Result, Session, Value};
use rudb_functions::{TableFunction, series_length};
use rudb_pipeline::{Progress, Stream};
use rudb_plan::{ExprRef, Plan, Slice};
use rudb_vector::{Chunk, VECTOR_SIZE};

use crate::prepared::{Prepared, Scratch};
use crate::rows;
use crate::schema::Schema;

/// One call of a series function per input row, with the input row beside every value it produced.
#[derive(Debug)]
pub(crate) struct LateralSeries {
    function: TableFunction,
    /// The arguments, read against a row of the input.
    args: Prepared,
    /// The input's columns followed by the one column a series produces.
    schema: Schema,
    types: Vec<LogicalType>,
    cancel: Cancel,
}

/// Where one instance of the operator is in the input chunk it was given.
#[derive(Debug)]
pub(crate) struct Calling {
    scratch: Scratch,
    /// The input chunk being walked, held while there is any of it left to answer.
    input: Option<Chunk>,
    /// The call each input row resolved to, or nothing where an argument was NULL.
    ///
    /// Worked out for the whole chunk in one pass rather than per row, because the arguments are
    /// ordinary expressions over the input and the evaluator reads a chunk at a time.
    calls: Vec<Option<Call>>,
    row: usize,
    /// How many of the current row's values have already come out.
    ///
    /// One call can produce more rows than fit in a chunk, and `range(1000000)` on one input row is
    /// exactly that, so a row is not always finished by the call that started it.
    made: usize,
}

/// One resolved call, which is a start, a step and how many values there are.
#[derive(Debug, Clone, Copy)]
struct Call {
    start: i64,
    step: i64,
    rows: usize,
}

impl LateralSeries {
    /// Applies the session semantics to the arguments' prepared casts.
    #[must_use]
    pub(crate) fn in_session(mut self, session: &Session) -> Self {
        self.args = self.args.in_session(session);
        self
    }

    /// # Errors
    ///
    /// If the name is not a table function, if it is one this operator does not answer, or if an
    /// argument does not resolve against the input's schema. All three are failures of the plan and
    /// are found when the operator is built rather than on the first chunk.
    pub(crate) fn new(
        plan: &Plan,
        input: &Schema,
        index: u32,
        function: &str,
        args: Slice,
        columns: Slice,
        cancel: &Cancel,
    ) -> Result<Self> {
        let Some(function) = TableFunction::lookup(function) else {
            return Err(Error::internal(format!("a plan with a table function called {function}")));
        };
        if !matches!(function, TableFunction::Range | TableFunction::GenerateSeries) {
            return Err(Error::internal(format!(
                "{}() reading a LATERAL column, which only a series does",
                function.name()
            )));
        }
        // The plan's own field rather than one made up here, so the name is the one the binder gave
        // and a query that renamed the column still finds it. The type is checked rather than taken,
        // because what comes out of this is a BIGINT and a field that said otherwise would be a
        // chunk that does not match the schema above it.
        let fields = plan.field_list(columns).to_vec();
        let [field] = fields.as_slice() else {
            return Err(Error::internal(format!(
                "{}() with {} columns rather than one",
                function.name(),
                fields.len()
            )));
        };
        if field.ty != LogicalType::BigInt {
            return Err(Error::internal(format!(
                "{}() producing {} rather than BIGINT",
                function.name(),
                field.ty
            )));
        }
        let produced = Schema::numbered(fields.clone(), index);
        let schema = Schema::concat(input, &produced);
        let exprs: Vec<ExprRef> = plan.expr_list(args).to_vec();
        Ok(Self {
            function,
            args: Prepared::new(plan, &exprs, input)?,
            types: schema.types(),
            schema,
            cancel: cancel.clone(),
        })
    }

    /// What this produces, which is the input's columns and then the function's.
    pub(crate) fn schema(&self) -> &Schema {
        &self.schema
    }

    /// The call each row of `chunk` asks for, or nothing where it asks for no rows at all.
    ///
    /// A NULL in any argument gives no rows, which is what the uncorrelated form answers and is not
    /// the same as an error. A step of zero is an error, and it is raised from here rather than
    /// skipped, because a query that wrote one is asking for a sequence that does not exist.
    fn calls(&self, chunk: &Chunk, scratch: &mut Scratch) -> Result<Vec<Option<Call>>> {
        let mut evaluated = Vec::new();
        self.args.evaluate(chunk, scratch, &mut evaluated)?;
        let mut calls = Vec::with_capacity(chunk.len());
        // row at a time: a call is a start, a step and a length, and working those out is arithmetic
        // over at most three numbers that ends in a branch on how many arguments were written. There
        // is no kernel shape to it and there is one of these per input row rather than one per
        // output row, so the loop below this is where the rows actually are.
        for row in 0..chunk.len() {
            let mut given = Vec::with_capacity(evaluated.len());
            for vector in &evaluated {
                match vector.value_at(row) {
                    Value::Null => {
                        given.clear();
                        break;
                    }
                    Value::BigInt(n) => given.push(n),
                    other => {
                        return Err(Error::internal(format!(
                            "a table function argument bound as BIGINT arrived as {other}"
                        )));
                    }
                }
            }
            if given.len() != evaluated.len() {
                calls.push(None);
                continue;
            }
            let (start, stop, step) = match given.as_slice() {
                [stop] => (0, *stop, 1),
                [start, stop] => (*start, *stop, 1),
                [start, stop, step] => (*start, *stop, *step),
                _ => {
                    return Err(Error::internal(format!(
                        "{}() bound with {} arguments",
                        self.function.name(),
                        given.len()
                    )));
                }
            };
            let rows = series_length(self.function, start, stop, step)?;
            calls.push(Some(Call { start, step, rows }));
        }
        Ok(calls)
    }
}

impl Stream for LateralSeries {
    type Local = Calling;

    fn local(&self) -> Calling {
        Calling { scratch: self.args.scratch(), input: None, calls: Vec::new(), row: 0, made: 0 }
    }

    fn push(&self, chunk: &mut Chunk, local: &mut Calling) -> Result<Progress> {
        let input = match local.input.take() {
            // Being asked again, so the chunk holds what went downstream last time and the input
            // rows are the ones this instance kept.
            Some(input) => input,
            None => {
                local.calls = self.calls(chunk, &mut local.scratch)?;
                local.row = 0;
                local.made = 0;
                chunk.clone()
            }
        };
        let mut out: Vec<Vec<Value>> = Vec::new();
        while local.row < input.len() && out.len() < VECTOR_SIZE {
            // Once per input row, which is the granularity a join checks at and is the only place
            // in this operator that runs long. `range(1000000000)` on one row is one check and then
            // a million chunks, so the re-entry below is what the token actually sees.
            self.cancel.check()?;
            if let Some(call) = local.calls[local.row] {
                let left: Vec<Value> = input.row(local.row).collect();
                let room = VECTOR_SIZE - out.len();
                let end = (local.made + room).min(call.rows);
                for at in local.made..end {
                    let steps = i64::try_from(at).unwrap_or(i64::MAX);
                    let mut made = left.clone();
                    made.push(Value::BigInt(
                        call.start.saturating_add(call.step.saturating_mul(steps)),
                    ));
                    out.push(made);
                }
                if end < call.rows {
                    // A call with more values than fit in a chunk. The row stays where it is and the
                    // next call picks up from the value this one stopped at.
                    local.made = end;
                    break;
                }
                local.made = 0;
            }
            local.row += 1;
        }
        *chunk = rows::pack(&self.types, &out)?;
        if local.row < input.len() {
            local.input = Some(input);
            return Ok(Progress::Again);
        }
        Ok(Progress::More)
    }
}
