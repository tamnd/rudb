//! The operators that pass over their input one chunk at a time and never hold it.
//!
//! These three are what a pipeline is made of. None of them allocates anything proportional to the
//! input, none of them can block, and each one either hands its chunk on, narrows it or replaces
//! its columns. That is the property the morsel driven scheduler in section 7.2 needs, because a
//! morsel is a run of a scan pushed through every streaming operator above it by one thread.

use rudb_common::Result;
use rudb_plan::{ExprRef, Plan, Slice};
use rudb_vector::{Chunk, Selection};

use crate::operator::Operator;
use crate::prepared::{Prepared, Scratch};
use crate::schema::Schema;

/// Keeps the rows where a predicate is true.
///
/// True, not "not false". A null predicate drops the row, which is what makes `WHERE x <> 5` leave
/// out the rows where `x` is null.
///
/// The predicate is evaluated with [`Prepared::evaluate_filter`] rather than as an expression, so a
/// top level `AND` runs a conjunct at a time over the rows the conjuncts before it left and stops
/// the moment nothing is left. The difference on a four conjunct predicate is the difference between
/// reading every row four times and reading it once.
///
/// The kept rows become a selection over the chunk rather than a copy of it, which is section 7.1's
/// rule: a filter that keeps one row in a thousand costs the selection and not the payload. The
/// copying alternative is [`Chunk::compact`], and its documentation has the measurement that says
/// why a streaming filter is not the caller for it. A chunk that keeps nothing is skipped here
/// rather than handed on, because an empty chunk travelling up a deep pipeline is work every
/// operator above does for no rows.
#[derive(Debug)]
pub(crate) struct Filter<'a> {
    input: Box<dyn Operator + 'a>,
    predicate: Prepared,
    scratch: Scratch,
    schema: Schema,
}

impl<'a> Filter<'a> {
    /// # Errors
    ///
    /// If the predicate does not resolve against the input's schema, which is a failure of the plan
    /// and is now found when the operator is built rather than on the first chunk.
    pub(crate) fn new(
        plan: &'a Plan,
        input: Box<dyn Operator + 'a>,
        predicate: ExprRef,
    ) -> Result<Self> {
        let schema = input.schema().clone();
        let predicate = Prepared::one(plan, predicate, &schema)?;
        let scratch = predicate.scratch();
        Ok(Self { input, predicate, scratch, schema })
    }
}

impl Operator for Filter<'_> {
    fn schema(&self) -> &Schema {
        &self.schema
    }

    fn next(&mut self) -> Result<Option<Chunk>> {
        while let Some(chunk) = self.input.next()? {
            let kept = self.predicate.evaluate_filter(&chunk, &mut self.scratch)?;
            if kept.is_empty() {
                continue;
            }
            if kept.len() == chunk.len() {
                return Ok(Some(chunk));
            }
            return Ok(Some(chunk.select(&kept)?));
        }
        Ok(None)
    }
}

/// Replaces the input's columns with a list of expressions.
#[derive(Debug)]
pub(crate) struct Project<'a> {
    input: Box<dyn Operator + 'a>,
    exprs: Prepared,
    scratch: Scratch,
    schema: Schema,
}

impl<'a> Project<'a> {
    /// A projection producing the plan's expressions under the plan's names.
    ///
    /// # Errors
    ///
    /// If there are not as many names as expressions, which [`Plan::validate`] already rejects and
    /// which is checked again here because this operator would otherwise produce a schema that is
    /// silently short.
    pub(crate) fn new(
        plan: &'a Plan,
        input: Box<dyn Operator + 'a>,
        index: u32,
        exprs: Slice,
        names: Slice,
    ) -> Result<Self> {
        let exprs: Vec<ExprRef> = plan.expr_list(exprs).to_vec();
        let names = plan.name_list(names);
        if names.len() != exprs.len() {
            return Err(rudb_common::Error::internal(format!(
                "a projection of {} expressions under {} names",
                exprs.len(),
                names.len()
            )));
        }
        let fields = exprs
            .iter()
            .zip(names)
            .map(|(&expr, &name)| {
                rudb_common::Field::new(plan.string(name), plan.expr_type(expr).clone())
            })
            .collect();
        let input_schema = input.schema().clone();
        let exprs = Prepared::new(plan, &exprs, &input_schema)?;
        let scratch = exprs.scratch();
        Ok(Self { input, exprs, scratch, schema: Schema::numbered(fields, index) })
    }
}

impl Operator for Project<'_> {
    fn schema(&self) -> &Schema {
        &self.schema
    }

    fn next(&mut self) -> Result<Option<Chunk>> {
        let Some(chunk) = self.input.next()? else {
            return Ok(None);
        };
        let mut columns = Vec::with_capacity(self.exprs.len());
        self.exprs.evaluate(&chunk, &mut self.scratch, &mut columns)?;
        Ok(Some(Chunk::with_rows(columns, chunk.len())?))
    }
}

/// Skips `offset` rows and then emits at most `count` of them.
///
/// The offset is consumed a row at a time rather than a chunk at a time, because an offset that
/// falls in the middle of a chunk is the ordinary case and rounding it to a chunk boundary is a
/// wrong answer. When the count is reached the input is dropped rather than drained, which is what
/// makes `LIMIT 10` over a large table stop early instead of scanning it.
#[derive(Debug)]
pub(crate) struct Limit<'a> {
    input: Box<dyn Operator + 'a>,
    schema: Schema,
    count: Option<u64>,
    offset: u64,
    skipped: u64,
    emitted: u64,
}

impl<'a> Limit<'a> {
    pub(crate) fn new(input: Box<dyn Operator + 'a>, count: Option<u64>, offset: u64) -> Self {
        let schema = input.schema().clone();
        Self { input, schema, count, offset, skipped: 0, emitted: 0 }
    }

    /// How many rows of a chunk are still wanted, given what has already been emitted.
    fn room(&self) -> Option<u64> {
        self.count.map(|count| count.saturating_sub(self.emitted))
    }
}

impl Operator for Limit<'_> {
    fn schema(&self) -> &Schema {
        &self.schema
    }

    fn next(&mut self) -> Result<Option<Chunk>> {
        loop {
            if self.room() == Some(0) {
                return Ok(None);
            }
            let Some(chunk) = self.input.next()? else {
                return Ok(None);
            };
            let rows = chunk.len() as u64;
            let skipping = (self.offset - self.skipped).min(rows);
            self.skipped += skipping;
            let available = rows - skipping;
            if available == 0 {
                continue;
            }
            let taking = match self.room() {
                Some(room) => room.min(available),
                None => available,
            };
            self.emitted += taking;
            if skipping == 0 && taking == rows {
                return Ok(Some(chunk));
            }
            let mut kept = Selection::with_capacity(taking as usize);
            for row in skipping..skipping + taking {
                kept.push(row as usize);
            }
            return Ok(Some(chunk.select(&kept)?));
        }
    }
}
