//! `json_each` and `json_tree`, which turn the values in a document into rows of their own with the
//! rest of the row beside them.
//!
//! Both spellings come here the way they do for `unnest`: `FROM t, json_each(t.j)` is a lateral
//! call over the rows of `t`, and `FROM json_each('[1, 2]')` is the same call over the single row
//! of a `FROM` with nothing in it. A row whose document or path is null makes no rows, and so does
//! a path that picks nothing.

use rudb_common::{Cancel, Error, LogicalType, Result, Session, Value};
use rudb_functions::{TableFunction, json_walk_fields};
use rudb_kernels::json::{Entry, entries};
use rudb_pipeline::{Progress, Stream};
use rudb_plan::{ExprRef, Plan, Slice};
use rudb_vector::{Chunk, VECTOR_SIZE};

use crate::prepared::{Prepared, Scratch};
use crate::rows;
use crate::schema::Schema;

/// The document and the path one row walks.
type Call = (String, Option<String>);

/// One walk of one document per input row, with the input row beside every value it produced.
#[derive(Debug)]
pub(crate) struct LateralJson {
    /// Whether this is `json_tree`, which walks everything under the start and not only the values
    /// directly inside it.
    tree: bool,
    /// The document and the path, read against a row of the input.
    args: Prepared,
    /// Which of the eight columns a walk produces are wanted, in the order they are, since a column
    /// nothing reads is dropped from the plan before this is built.
    picks: Vec<usize>,
    /// The input's columns followed by the ones picked.
    schema: Schema,
    types: Vec<LogicalType>,
    /// Whether a last column numbers the values of each document from 1, which is
    /// `WITH ORDINALITY`.
    ordinality: bool,
    cancel: Cancel,
}

/// Where one instance of the operator is in the input chunk it was given.
#[derive(Debug)]
pub(crate) struct Walking {
    scratch: Scratch,
    /// The input chunk being walked, held while there is any of it left to answer.
    input: Option<Chunk>,
    /// Each row's document and path, or nothing where either is null.
    calls: Vec<Option<Call>>,
    row: usize,
    /// The rows the current input row makes, worked out when the walk reaches it.
    walked: Vec<Entry>,
    /// How many of those have already come out.
    made: usize,
}

impl LateralJson {
    /// Applies the session semantics to the arguments' prepared casts.
    #[must_use]
    pub(crate) fn in_session(mut self, session: &Session) -> Self {
        self.args = self.args.in_session(session);
        self
    }

    /// # Errors
    ///
    /// If the name is not one of the two walks, or if an argument does not resolve against the
    /// input's schema. Both are failures of the plan.
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn new(
        plan: &Plan,
        input: &Schema,
        index: u32,
        function: &str,
        args: Slice,
        columns: Slice,
        ordinality: bool,
        cancel: &Cancel,
    ) -> Result<Self> {
        let tree = match TableFunction::lookup(function) {
            Some(TableFunction::JsonEach) => false,
            Some(TableFunction::JsonTree) => true,
            _ => return Err(Error::internal(format!("{function}() walking a document"))),
        };
        let fields = plan.field_list(columns).to_vec();
        let all = json_walk_fields();
        let walked = fields.len().saturating_sub(usize::from(ordinality));
        let picks = fields[..walked]
            .iter()
            .map(|field| {
                all.iter().position(|held| held.name == field.name).ok_or_else(|| {
                    Error::internal(format!("{function}() has no column named {}", field.name))
                })
            })
            .collect::<Result<Vec<_>>>()?;
        let produced = Schema::numbered(fields, index);
        let schema = Schema::concat(input, &produced);
        let exprs: Vec<ExprRef> = plan.expr_list(args).to_vec();
        Ok(Self {
            tree,
            picks,
            args: Prepared::new(plan, &exprs, input)?,
            types: schema.types(),
            schema,
            ordinality,
            cancel: cancel.clone(),
        })
    }

    /// What this produces, which is the input's columns and then the walk's.
    pub(crate) fn schema(&self) -> &Schema {
        &self.schema
    }

    /// The document and the path each row of `chunk` walks, or nothing where it walks nothing.
    fn calls(&self, chunk: &Chunk, scratch: &mut Scratch) -> Result<Vec<Option<Call>>> {
        let mut evaluated = Vec::new();
        self.args.evaluate(chunk, scratch, &mut evaluated)?;
        let mut calls = Vec::with_capacity(chunk.len());
        // row at a time: each row is one document that is parsed and walked as a whole, which costs
        // far more than reading its two arguments as values.
        for row in 0..chunk.len() {
            let document = match evaluated.first().map(|vector| vector.value_at(row)) {
                Some(Value::Varchar(text)) => text,
                _ => {
                    calls.push(None);
                    continue;
                }
            };
            let path = match evaluated.get(1).map(|vector| vector.value_at(row)) {
                None => None,
                Some(Value::Varchar(text)) => Some(text),
                Some(_) => {
                    calls.push(None);
                    continue;
                }
            };
            calls.push(Some((document, path)));
        }
        Ok(calls)
    }
}

/// The eight values one entry is, in the order of the columns.
fn entry_values(entry: &Entry) -> [Value; 8] {
    let text = |text: &Option<String>| text.clone().map_or(Value::Null, Value::Varchar);
    [
        text(&entry.key),
        Value::Varchar(entry.value.clone()),
        Value::Varchar(entry.kind.to_string()),
        text(&entry.atom),
        Value::UBigInt(entry.id),
        entry.parent.map_or(Value::Null, Value::UBigInt),
        Value::Varchar(entry.fullkey.clone()),
        Value::Varchar(entry.path.clone()),
    ]
}

impl Stream for LateralJson {
    type Local = Walking;

    fn local(&self) -> Walking {
        Walking {
            scratch: self.args.scratch(),
            input: None,
            calls: Vec::new(),
            row: 0,
            walked: Vec::new(),
            made: 0,
        }
    }

    fn push(&self, chunk: &mut Chunk, local: &mut Walking) -> Result<Progress> {
        let input = match local.input.take() {
            // Being asked again, so the chunk holds what went downstream last time and the input
            // rows are the ones this instance kept.
            Some(input) => input,
            None => {
                local.calls = self.calls(chunk, &mut local.scratch)?;
                local.row = 0;
                local.walked.clear();
                local.made = 0;
                chunk.clone()
            }
        };
        let mut out: Vec<Vec<Value>> = Vec::new();
        while local.row < input.len() && out.len() < VECTOR_SIZE {
            self.cancel.check()?;
            if local.made == 0
                && let Some((document, path)) = &local.calls[local.row]
            {
                local.walked = entries(document, path.as_deref(), self.tree)?;
            }
            if local.calls[local.row].is_some() {
                let left: Vec<Value> = input.row(local.row).collect();
                let room = VECTOR_SIZE - out.len();
                let end = (local.made + room).min(local.walked.len());
                for (at, entry) in local.walked[local.made..end].iter().enumerate() {
                    let mut made = left.clone();
                    let values = entry_values(entry);
                    made.extend(self.picks.iter().map(|&at| values[at].clone()));
                    if self.ordinality {
                        let number = local.made + at + 1;
                        made.push(Value::BigInt(i64::try_from(number).unwrap_or(i64::MAX)));
                    }
                    out.push(made);
                }
                if end < local.walked.len() {
                    // A document with more values than fit in a chunk. The row stays where it is
                    // and the next call picks up from the value this one stopped at.
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
