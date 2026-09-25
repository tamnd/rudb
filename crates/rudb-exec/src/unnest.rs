//! `unnest`, which turns every element of a list into a row of its own with the rest of the row
//! beside it.
//!
//! One operator answers both spellings. `SELECT unnest(l) FROM t` is bound as a lateral call over
//! the rows the select list reads, one level of it per level of nesting it takes apart, and
//! `FROM unnest(l)` is the same call over the single row of a `FROM` with nothing in it. Several
//! lists in one call are taken apart side by side, the way the pin zips the unnests of one select
//! list, so a row makes as many rows as its longest list has elements and a shorter list is null for
//! the rest. A null list and an empty one both have no elements, so a row whose lists are all like
//! that makes no rows at all.
//!
//! Nothing is copied out of the lists a value at a time. The input row a produced row came from is
//! a code into the input chunk, and an element is a row id into the list's child, so a chunk of
//! output is one pass writing ids and one gather per list.

use std::sync::Arc;

use rudb_common::{Cancel, Error, LogicalType, Result, Session, Value};
use rudb_pipeline::{Progress, Stream};
use rudb_plan::{ExprRef, Plan, Slice};
use rudb_vector::selection::Selection;
use rudb_vector::vector::NO_ROW;
use rudb_vector::{Chunk, VECTOR_SIZE, Vector};

use crate::prepared::{Prepared, Scratch};
use crate::schema::Schema;

/// One `unnest` of one or more lists per input row, with the input row beside every element.
#[derive(Debug)]
pub(crate) struct LateralUnnest {
    /// The lists, read against a row of the input.
    args: Prepared,
    /// The type of each element column, in the order of the arguments.
    elements: Vec<LogicalType>,
    /// The input's columns followed by one column per list.
    schema: Schema,
    cancel: Cancel,
}

/// One list argument over the chunk being walked.
#[derive(Debug)]
struct Taken {
    /// Where each row's elements start in `child` and how many there are, with no elements for a
    /// null row.
    entries: Vec<(u32, u32)>,
    child: Arc<Vector>,
}

/// Where one instance of the operator is in the input chunk it was given.
#[derive(Debug)]
pub(crate) struct Unnesting {
    scratch: Scratch,
    /// The input chunk being walked, held while there is any of it left to answer.
    input: Option<Chunk>,
    /// Each argument's lists over that chunk, or nothing for an argument that is always null.
    lists: Vec<Option<Taken>>,
    /// How many rows each input row makes, which is the length of its longest list.
    counts: Vec<u32>,
    row: usize,
    /// How many of the current row's rows have already come out.
    made: usize,
}

impl LateralUnnest {
    /// Applies the session semantics to the arguments' prepared casts.
    #[must_use]
    pub(crate) fn in_session(mut self, session: &Session) -> Self {
        self.args = self.args.in_session(session);
        self
    }

    /// # Errors
    ///
    /// If the columns do not match the arguments one for one, or if an argument does not resolve
    /// against the input's schema. Both are failures of the plan.
    pub(crate) fn new(
        plan: &Plan,
        input: &Schema,
        index: u32,
        args: Slice,
        columns: Slice,
        cancel: &Cancel,
    ) -> Result<Self> {
        let fields = plan.field_list(columns).to_vec();
        let exprs: Vec<ExprRef> = plan.expr_list(args).to_vec();
        if fields.len() != exprs.len() {
            return Err(Error::internal(format!(
                "unnest() of {} lists with {} columns",
                exprs.len(),
                fields.len()
            )));
        }
        let elements = fields.iter().map(|field| field.ty.clone()).collect();
        let produced = Schema::numbered(fields, index);
        let schema = Schema::concat(input, &produced);
        Ok(Self {
            args: Prepared::new(plan, &exprs, input)?,
            elements,
            schema,
            cancel: cancel.clone(),
        })
    }

    /// What this produces, which is the input's columns and then one per list.
    pub(crate) fn schema(&self) -> &Schema {
        &self.schema
    }

    /// Takes the lists of a fresh input chunk apart into entries and children, and counts the rows
    /// each input row makes.
    fn prepare(&self, chunk: &Chunk, local: &mut Unnesting) -> Result<()> {
        let mut evaluated = Vec::new();
        self.args.evaluate(chunk, &mut local.scratch, &mut evaluated)?;
        local.lists.clear();
        local.counts.clear();
        local.counts.resize(chunk.len(), 0);
        for vector in evaluated {
            let taken = taken(vector)?;
            if let Some(taken) = &taken {
                for (count, &(_, len)) in local.counts.iter_mut().zip(&taken.entries) {
                    *count = (*count).max(len);
                }
            }
            local.lists.push(taken);
        }
        local.row = 0;
        local.made = 0;
        Ok(())
    }
}

/// A list vector as its entries and its child, or `None` for one that is not a list, which is the
/// untyped null `unnest(NULL)` is.
fn taken(vector: Vector) -> Result<Option<Taken>> {
    if !matches!(vector.logical_type(), LogicalType::List(_)) {
        return Ok(None);
    }
    let vector = if vector.list_parts().is_some() { vector } else { vector.into_flat()? };
    let vector = if vector.list_parts().is_some() {
        vector
    } else {
        // row at a time: a list that came out of the evaluator in some other form, which it does
        // not today, is rebuilt once here so that the walk below has entries to read.
        let values: Vec<Value> = (0..vector.len()).map(|row| vector.value_at(row)).collect();
        Vector::from_values(vector.logical_type().clone(), &values)?
    };
    let Some((entries, child)) = vector.list_parts() else {
        return Err(Error::internal("a list vector without entries"));
    };
    let entries = entries
        .iter()
        .enumerate()
        .map(|(row, &entry)| if vector.is_null_at(row) { (0, 0) } else { entry })
        .collect();
    Ok(Some(Taken { entries, child: Arc::new(child.clone()) }))
}

impl Stream for LateralUnnest {
    type Local = Unnesting;

    fn local(&self) -> Unnesting {
        Unnesting {
            scratch: self.args.scratch(),
            input: None,
            lists: Vec::new(),
            counts: Vec::new(),
            row: 0,
            made: 0,
        }
    }

    fn push(&self, chunk: &mut Chunk, local: &mut Unnesting) -> Result<Progress> {
        self.cancel.check()?;
        let input = match local.input.take() {
            // Being asked again, so the chunk holds what went downstream last time and the input
            // rows are the ones this instance kept.
            Some(input) => input,
            None => {
                self.prepare(chunk, local)?;
                chunk.clone()
            }
        };
        let mut rows = Vec::new();
        let mut rids: Vec<Vec<u32>> = vec![Vec::new(); local.lists.len()];
        while local.row < input.len() && rows.len() < VECTOR_SIZE {
            let count = local.counts[local.row] as usize;
            let end = (local.made + VECTOR_SIZE - rows.len()).min(count);
            let row = u32::try_from(local.row).map_err(|_| Error::internal("a chunk row"))?;
            for at in local.made..end {
                rows.push(row);
                for (list, ids) in local.lists.iter().zip(&mut rids) {
                    let id = match list {
                        Some(taken) => {
                            let (start, len) = taken.entries[local.row];
                            let at = u32::try_from(at).map_err(|_| Error::internal("a list"))?;
                            if at < len { start + at } else { NO_ROW }
                        }
                        None => NO_ROW,
                    };
                    ids.push(id);
                }
            }
            if end < count {
                // A row with more elements than fit in what is left of the chunk. The row stays
                // where it is and the next call picks up from the element this one stopped at.
                local.made = end;
                break;
            }
            local.made = 0;
            local.row += 1;
        }
        let len = rows.len();
        let kept = input.clone().select(&Selection::from_indices(rows))?;
        let mut columns = kept.columns().to_vec();
        for ((list, ids), element) in local.lists.iter().zip(rids).zip(&self.elements) {
            columns.push(match list {
                Some(taken) => {
                    Vector::gathered(Arc::clone(&taken.child), Arc::new(ids))?.into_flat()?
                }
                None => Vector::constant(element.clone(), Value::Null, len),
            });
        }
        *chunk = Chunk::with_rows(columns, len)?;
        if local.row < input.len() {
            local.input = Some(input);
            return Ok(Progress::Again);
        }
        Ok(Progress::More)
    }
}
