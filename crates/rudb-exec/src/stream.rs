//! The operators that pass over their input one chunk at a time and never hold it.
//!
//! These three are what a pipeline is made of. None of them allocates anything proportional to the
//! input, none of them can block, and each one either hands its chunk on, narrows it or replaces
//! its columns. That is the property the morsel driven scheduler in section 7.2 needs, because a
//! morsel is a run of a scan pushed through every streaming operator above it by one thread.
//!
//! All three are [`Stream`] implementations, which means they take `&self` and are handed the
//! mutable part separately. A filter's mutable part is the scratch space its predicate evaluates
//! into and a limit's is the two counters, and naming them is what lets one of these be
//! instantiated on thirty two threads later without copying the predicate thirty two times. The
//! tree in `build.rs` is still a pull tree, so [`Streamed`](crate::adapt::Streamed) drives them
//! from above until the whole engine pushes.

use rudb_common::{Field, Result};
use rudb_pipeline::{Progress, Stream};
use rudb_plan::{ExprRef, Plan, Slice};
use rudb_vector::{Chunk, Selection};

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
/// why a streaming filter is not the caller for it. A chunk that keeps nothing is left empty rather
/// than passed on with rows in it, and whoever is driving skips it, because an empty chunk
/// travelling up a deep pipeline is work every operator above does for no rows.
#[derive(Debug)]
pub(crate) struct Filter {
    predicate: Prepared,
}

impl Filter {
    /// # Errors
    ///
    /// If the predicate does not resolve against the input's schema, which is a failure of the plan
    /// and is found when the operator is built rather than on the first chunk.
    pub(crate) fn new(plan: &Plan, predicate: ExprRef, input: &Schema) -> Result<Self> {
        Ok(Self { predicate: Prepared::one(plan, predicate, input)? })
    }
}

impl Stream for Filter {
    type Local = Scratch;

    fn local(&self) -> Scratch {
        self.predicate.scratch()
    }

    fn push(&self, chunk: &mut Chunk, scratch: &mut Scratch) -> Result<Progress> {
        let kept = self.predicate.evaluate_filter(chunk, scratch)?;
        if kept.len() != chunk.len() {
            keep(chunk, &kept)?;
        }
        Ok(Progress::More)
    }
}

/// Replaces the input's columns with a list of expressions.
#[derive(Debug)]
pub(crate) struct Project {
    exprs: Prepared,
    schema: Schema,
}

impl Project {
    /// A projection producing the plan's expressions under the plan's names.
    ///
    /// # Errors
    ///
    /// If there are not as many names as expressions, which [`Plan::validate`] already rejects and
    /// which is checked again here because this operator would otherwise produce a schema that is
    /// silently short.
    pub(crate) fn new(
        plan: &Plan,
        input: &Schema,
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
            .map(|(&expr, &name)| Field::new(plan.string(name), plan.expr_type(expr).clone()))
            .collect();
        let exprs = Prepared::new(plan, &exprs, input)?;
        Ok(Self { exprs, schema: Schema::numbered(fields, index) })
    }

    /// The columns this projection produces.
    pub(crate) fn schema(&self) -> &Schema {
        &self.schema
    }
}

impl Stream for Project {
    type Local = Scratch;

    fn local(&self) -> Scratch {
        self.exprs.scratch()
    }

    fn push(&self, chunk: &mut Chunk, scratch: &mut Scratch) -> Result<Progress> {
        let mut columns = Vec::with_capacity(self.exprs.len());
        self.exprs.evaluate(chunk, scratch, &mut columns)?;
        *chunk = Chunk::with_rows(columns, chunk.len())?;
        Ok(Progress::More)
    }
}

/// Skips `offset` rows and then emits at most `count` of them.
///
/// The offset is consumed a row at a time rather than a chunk at a time, because an offset that
/// falls in the middle of a chunk is the ordinary case and rounding it to a chunk boundary is a
/// wrong answer. The chunk that reaches the count comes back with [`Progress::Done`] on it, which
/// is how `LIMIT 10` over a large table stops the scan instead of reading rows in order to throw
/// them away.
#[derive(Debug)]
pub(crate) struct Limit {
    count: Option<u64>,
    offset: u64,
}

/// How much of the limit one instance has used up.
#[derive(Debug, Default)]
pub(crate) struct Taken {
    skipped: u64,
    emitted: u64,
}

impl Limit {
    pub(crate) fn new(count: Option<u64>, offset: u64) -> Self {
        Self { count, offset }
    }

    /// How many rows are still wanted, given what has already been emitted.
    fn room(&self, taken: &Taken) -> Option<u64> {
        self.count.map(|count| count.saturating_sub(taken.emitted))
    }
}

impl Stream for Limit {
    type Local = Taken;

    fn local(&self) -> Taken {
        Taken::default()
    }

    fn push(&self, chunk: &mut Chunk, taken: &mut Taken) -> Result<Progress> {
        let rows = chunk.len() as u64;
        let skipping = (self.offset - taken.skipped).min(rows);
        taken.skipped += skipping;
        let available = rows - skipping;
        let taking = match self.room(taken) {
            Some(room) => room.min(available),
            None => available,
        };
        taken.emitted += taking;
        if skipping != 0 || taking != rows {
            let mut kept = Selection::with_capacity(taking as usize);
            for row in skipping..skipping + taking {
                kept.push(row as usize);
            }
            keep(chunk, &kept)?;
        }
        match self.room(taken) {
            Some(0) => Ok(Progress::Done),
            _ => Ok(Progress::More),
        }
    }
}

/// Narrow a chunk to the rows a selection kept, in place.
///
/// [`Chunk::select`] takes the chunk by value, because taking it by value is what lets it move the
/// payload into the new vectors rather than copy it, and a push operator has a `&mut` and not a
/// value. So the chunk is swapped out for an empty one, narrowed, and put back. The empty one is
/// never observed, since a failure here fails the query.
fn keep(chunk: &mut Chunk, kept: &Selection) -> Result<()> {
    let whole = std::mem::replace(chunk, Chunk::empty(&[]));
    *chunk = whole.select(kept)?;
    Ok(())
}

/// The limit driven through the trait rather than through a plan.
///
/// The plan level tests are in `tests.rs` and they go through `build`, which is the right place to
/// check that `LIMIT 3 OFFSET 1` answers with the rows it should. These check the part only the
/// trait has, which is when [`Progress::Done`] comes back, because that is the signal that stops a
/// scan and nothing above the operator can see it once the answer has been assembled.
#[cfg(test)]
mod tests {
    use rudb_common::{LogicalType, Value};
    use rudb_vector::{Data, Vector};

    use super::{Limit, Progress, Stream};
    use rudb_vector::Chunk;

    fn chunk(values: &[i32]) -> Chunk {
        let column = Vector::flat(LogicalType::Integer, Data::Int32(values.to_vec().into()))
            .expect("integers are an i32 layout");
        Chunk::new(vec![column]).expect("one column is one length")
    }

    fn rows(chunk: &Chunk) -> Vec<Value> {
        (0..chunk.len()).map(|row| chunk.value_at(row, 0)).collect()
    }

    #[test]
    fn the_chunk_that_fills_the_count_is_the_one_that_says_done() {
        let limit = Limit::new(Some(3), 0);
        let mut taken = limit.local();

        let mut first = chunk(&[1, 2]);
        assert_eq!(limit.push(&mut first, &mut taken).expect("two rows fit"), Progress::More);
        assert_eq!(rows(&first), vec![Value::Integer(1), Value::Integer(2)]);

        let mut second = chunk(&[3, 4]);
        assert_eq!(limit.push(&mut second, &mut taken).expect("one row fits"), Progress::Done);
        assert_eq!(rows(&second), vec![Value::Integer(3)]);
    }

    #[test]
    fn an_offset_that_falls_inside_a_chunk_is_counted_in_rows() {
        let limit = Limit::new(None, 3);
        let mut taken = limit.local();

        let mut first = chunk(&[1, 2]);
        assert_eq!(limit.push(&mut first, &mut taken).expect("all skipped"), Progress::More);
        assert!(first.is_empty());

        let mut second = chunk(&[3, 4, 5]);
        assert_eq!(limit.push(&mut second, &mut taken).expect("one more skipped"), Progress::More);
        assert_eq!(rows(&second), vec![Value::Integer(4), Value::Integer(5)]);
    }

    /// `LIMIT 0` is a query that reads nothing, and it is worth its own test because the count is
    /// reached before a row has been seen, which is the one path where the operator is finished on
    /// the call that starts it.
    #[test]
    fn a_limit_of_nothing_is_done_on_the_first_chunk() {
        let limit = Limit::new(Some(0), 0);
        let mut taken = limit.local();
        let mut first = chunk(&[1, 2]);
        assert_eq!(limit.push(&mut first, &mut taken).expect("nothing wanted"), Progress::Done);
        assert!(first.is_empty());
    }

    /// An instance's counters are its own, which is what makes the operator shareable. Two locals
    /// off one limit both get their own three rows.
    #[test]
    fn two_instances_of_one_limit_do_not_share_a_count() {
        let limit = Limit::new(Some(3), 0);
        let mut one = limit.local();
        let mut two = limit.local();

        let mut first = chunk(&[1, 2, 3]);
        assert_eq!(limit.push(&mut first, &mut one).expect("three rows"), Progress::Done);
        let mut second = chunk(&[4, 5, 6]);
        assert_eq!(limit.push(&mut second, &mut two).expect("three rows"), Progress::Done);
        assert_eq!(rows(&second), vec![Value::Integer(4), Value::Integer(5), Value::Integer(6)]);
    }
}
