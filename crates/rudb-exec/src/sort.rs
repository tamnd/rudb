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

use rudb_common::{Error, Result, Value};
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
}

impl<'a> Sort<'a> {
    pub(crate) fn new(plan: &'a Plan, input: Box<dyn Operator + 'a>, keys: Slice) -> Self {
        let schema = input.schema().clone();
        Self {
            input,
            plan,
            keys: plan.sort_key_list(keys).to_vec(),
            schema,
            built: false,
            chunks: Vec::new(),
            at: 0,
        }
    }

    fn build(&mut self) -> Result<()> {
        let exprs: Vec<_> = self.keys.iter().map(|key| key.expr).collect();
        let mut sortable: Vec<(Vec<Value>, Vec<Value>)> = Vec::new();
        while let Some(chunk) = self.input.next()? {
            let keys = evaluate_all(self.plan, &exprs, &self.schema, &chunk)?;
            for row in 0..chunk.len() {
                let key = keys.iter().map(|column| column.value_at(row)).collect();
                sortable.push((key, chunk.row(row).collect()));
            }
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
        self.chunks = rows::chunks(&self.schema.types(), &ordered)?;
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
