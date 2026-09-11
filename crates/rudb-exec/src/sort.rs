//! Sorting.
//!
//! A pipeline breaker: nothing can be emitted until the last row has arrived, because the last row
//! of the input can be the first row of the output. It reads everything, sorts, and then hands the
//! result out a chunk at a time.
//!
//! The sort is Rust's `sort_by`, which is stable. Stability is not something SQL promises and it is
//! kept anyway, because `ORDER BY a` over rows that tie on `a` producing a different order on two
//! runs of the same query is the kind of difference that makes a compatibility diff useless.

use std::cmp::Ordering;

use rudb_common::{Error, Memory, Reservation, Result, Value};
use rudb_plan::{Plan, Slice, SortKey};
use rudb_vector::Chunk;

use crate::expr::evaluate_all;
use crate::operator::Operator;
use crate::rows;
use crate::schema::Schema;

/// An ordering over the input.
#[derive(Debug)]
pub(crate) struct Sort<'a> {
    input: Box<dyn Operator + 'a>,
    plan: &'a Plan,
    keys: Vec<SortKey>,
    schema: Schema,
    built: bool,
    chunks: Vec<Chunk>,
    at: usize,
    memory: Memory,
    /// What the sorted chunks are charged, held for as long as this operator holds them.
    held: Reservation,
}

impl<'a> Sort<'a> {
    pub(crate) fn new(
        plan: &'a Plan,
        input: Box<dyn Operator + 'a>,
        keys: Slice,
        memory: &Memory,
    ) -> Self {
        let schema = input.schema().clone();
        Self {
            input,
            plan,
            keys: plan.sort_key_list(keys).to_vec(),
            schema,
            built: false,
            chunks: Vec::new(),
            at: 0,
            memory: memory.clone(),
            held: memory.reservation(),
        }
    }

    fn build(&mut self) -> Result<()> {
        let exprs: Vec<_> = self.keys.iter().map(|key| key.expr).collect();
        // The keys and the rows waiting to be sorted, charged separately from the sorted chunks
        // below, because this one is given back the moment the sort is done with it and the other
        // is held for as long as anybody can ask this operator for a chunk.
        let mut scratch = self.memory.reservation();
        let mut sortable: Vec<(Vec<Value>, Vec<Value>)> = Vec::new();
        while let Some(chunk) = self.input.next()? {
            let keys = evaluate_all(self.plan, &exprs, &self.schema, &chunk)?;
            let mut taken = 0;
            // row at a time: 2i (#63) sorts a normalized key that is one comparable byte string a
            // row rather than a `Vec<Value>`, and moves the payload by index at the end instead of
            // carrying a copy of every row through the sort.
            for row in 0..chunk.len() {
                let key: Vec<Value> = keys.iter().map(|column| column.value_at(row)).collect();
                let values: Vec<Value> = chunk.row(row).collect();
                taken += rows::footprint(&key) + rows::footprint(&values);
                sortable.push((key, values));
            }
            scratch.grow(taken)?;
        }
        let mut failure: Option<Error> = None;
        sortable.sort_by(|left, right| {
            for (at, key) in self.keys.iter().enumerate() {
                let ordering = match rank(&left.0[at], &right.0[at], *key) {
                    Ok(ordering) => ordering,
                    Err(error) => {
                        failure.get_or_insert(error);
                        Ordering::Equal
                    }
                };
                if ordering != Ordering::Equal {
                    return ordering;
                }
            }
            Ordering::Equal
        });
        if let Some(error) = failure {
            return Err(error);
        }
        let ordered: Vec<Vec<Value>> = sortable.into_iter().map(|(_, row)| row).collect();
        self.chunks = rows::chunks(&self.schema.types(), &ordered, &mut self.held)?;
        Ok(())
    }
}

/// Where two values sit relative to each other under one sort key.
///
/// The direction and the null placement are independent, which is the detail worth being careful
/// about. `ORDER BY x DESC NULLS LAST` is not `ORDER BY x NULLS FIRST` reversed: reversing the
/// whole comparison would move the nulls too, and DuckDB's answer keeps them where the query put
/// them. So the direction is applied to the comparison of two values and never to the rule that
/// places a null.
fn rank(left: &Value, right: &Value, key: SortKey) -> Result<Ordering> {
    match (left.is_null(), right.is_null()) {
        (true, true) => Ok(Ordering::Equal),
        (true, false) => Ok(if key.nulls_first { Ordering::Less } else { Ordering::Greater }),
        (false, true) => Ok(if key.nulls_first { Ordering::Greater } else { Ordering::Less }),
        (false, false) => {
            let ordering = rudb_kernels::order(left, right)?;
            Ok(if key.descending { ordering.reverse() } else { ordering })
        }
    }
}

impl Operator for Sort<'_> {
    fn schema(&self) -> &Schema {
        &self.schema
    }

    fn next(&mut self) -> Result<Option<Chunk>> {
        if !self.built {
            self.build()?;
            self.built = true;
        }
        if self.at >= self.chunks.len() {
            return Ok(None);
        }
        let chunk = self.chunks[self.at].clone();
        self.at += 1;
        Ok(Some(chunk))
    }
}
