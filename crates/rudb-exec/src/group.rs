//! Grouping and duplicate elimination.
//!
//! Both are hash tables over [`Key`], which is what makes them agree about what one row is. A
//! `GROUP BY x` that put two nulls in two groups and a `SELECT DISTINCT x` that collapsed them into
//! one would be two answers to the same question, and the only way to be sure that never happens is
//! for both to ask the same type.
//!
//! The table is a `HashMap` from key to slot alongside a `Vec` of keys in first arrival order, so
//! the output comes out in the order the groups were first seen. SQL does not promise that and
//! DuckDB does not either, but a deterministic order costs one push per group and makes a failing
//! test a diff instead of an investigation.

use std::collections::{HashMap, HashSet};

use rudb_common::{Error, Field, LogicalType, Result, Value};
use rudb_kernels::{Accumulator, is_true};
use rudb_plan::{Expr, ExprRef, Plan, Slice};
use rudb_vector::Chunk;

use crate::expr::{evaluate, evaluate_all};
use crate::key::Key;
use crate::operator::Operator;
use crate::rows;
use crate::schema::Schema;

/// One aggregate call, taken apart once when the operator is built.
#[derive(Debug, Clone)]
struct Call {
    name: String,
    args: Vec<ExprRef>,
    distinct: bool,
    filter: Option<ExprRef>,
    returns: LogicalType,
}

/// A grouped or ungrouped aggregation.
///
/// The output is the group expressions followed by the aggregates, which is what a binding into
/// this operator's table index means and what the binder assumed when it made one.
///
/// An ungrouped aggregate produces exactly one row even over an empty input. That is done by
/// creating the single empty group when the operator is built rather than when the first row
/// arrives, which is the whole of the difference between `SELECT count(*) FROM empty` answering
/// zero and answering nothing.
#[derive(Debug)]
pub(crate) struct Aggregate<'a> {
    input: Box<dyn Operator + 'a>,
    plan: &'a Plan,
    input_schema: Schema,
    groups: Vec<ExprRef>,
    calls: Vec<Call>,
    schema: Schema,
    built: bool,
    chunks: Vec<Chunk>,
    at: usize,
}

impl<'a> Aggregate<'a> {
    /// An aggregation over the plan's groups and aggregate calls.
    ///
    /// # Errors
    ///
    /// If an entry in the aggregate list is not an aggregate, which [`Plan::validate`] rejects and
    /// which is checked again here because this operator has no sensible behaviour if it is wrong.
    pub(crate) fn new(
        plan: &'a Plan,
        input: Box<dyn Operator + 'a>,
        index: u32,
        groups: Slice,
        aggregates: Slice,
    ) -> Result<Self> {
        let input_schema = input.schema().clone();
        let groups: Vec<ExprRef> = plan.expr_list(groups).to_vec();
        let mut calls = Vec::new();
        for &reference in plan.expr_list(aggregates) {
            let Expr::Aggregate { name, args, distinct, filter } = *plan.expr(reference) else {
                return Err(Error::internal(format!(
                    "expression {reference} is in the aggregate list of an Aggregate and is not an aggregate"
                )));
            };
            calls.push(Call {
                name: plan.string(name).to_string(),
                args: plan.expr_list(args).to_vec(),
                distinct,
                filter,
                returns: plan.expr_type(reference).clone(),
            });
        }
        let mut fields = Vec::with_capacity(groups.len() + calls.len());
        for (at, &group) in groups.iter().enumerate() {
            fields.push(Field::new(
                group_name(plan, group, &input_schema, at),
                plan.expr_type(group).clone(),
            ));
        }
        for call in &calls {
            fields.push(Field::new(call.name.clone(), call.returns.clone()));
        }
        let schema = Schema::numbered(fields, index);
        Ok(Self {
            input,
            plan,
            input_schema,
            groups,
            calls,
            schema,
            built: false,
            chunks: Vec::new(),
            at: 0,
        })
    }

    /// Reads the whole input and builds the hash table.
    fn build(&mut self) -> Result<()> {
        let mut order: Vec<Key> = Vec::new();
        let mut slots: HashMap<Key, usize> = HashMap::new();
        let mut states: Vec<Vec<Accumulator>> = Vec::new();
        let mut seen: Vec<Vec<HashSet<Key>>> = Vec::new();
        if self.groups.is_empty() {
            let key = Key(Vec::new());
            slots.insert(key.clone(), 0);
            order.push(key);
            states.push(self.fresh()?);
            seen.push(vec![HashSet::new(); self.calls.len()]);
        }
        while let Some(chunk) = self.input.next()? {
            let keys = evaluate_all(self.plan, &self.groups, &self.input_schema, &chunk)?;
            let mut arguments = Vec::with_capacity(self.calls.len());
            let mut filters = Vec::with_capacity(self.calls.len());
            for call in &self.calls {
                arguments.push(evaluate_all(self.plan, &call.args, &self.input_schema, &chunk)?);
                filters.push(match call.filter {
                    Some(filter) => Some(evaluate(self.plan, filter, &self.input_schema, &chunk)?),
                    None => None,
                });
            }
            for row in 0..chunk.len() {
                let key = Key(keys.iter().map(|column| column.value_at(row)).collect());
                let slot = match slots.get(&key) {
                    Some(&slot) => slot,
                    None => {
                        let slot = states.len();
                        slots.insert(key.clone(), slot);
                        order.push(key);
                        states.push(self.fresh()?);
                        seen.push(vec![HashSet::new(); self.calls.len()]);
                        slot
                    }
                };
                for (at, call) in self.calls.iter().enumerate() {
                    if let Some(flags) = &filters[at] {
                        if !is_true(&flags.value_at(row)) {
                            continue;
                        }
                    }
                    let args: Vec<Value> =
                        arguments[at].iter().map(|column| column.value_at(row)).collect();
                    if call.distinct && !seen[slot][at].insert(Key(args.clone())) {
                        continue;
                    }
                    states[slot][at].update(&args)?;
                }
            }
        }
        let mut out = Vec::with_capacity(order.len());
        for (slot, key) in order.into_iter().enumerate() {
            let mut row = key.0;
            for accumulator in &states[slot] {
                row.push(accumulator.finish()?);
            }
            out.push(row);
        }
        self.chunks = rows::chunks(&self.schema.types(), &out)?;
        Ok(())
    }

    /// A fresh accumulator per call, which is what one new group costs.
    fn fresh(&self) -> Result<Vec<Accumulator>> {
        self.calls.iter().map(|call| Accumulator::new(&call.name, &call.returns)).collect()
    }
}

impl Operator for Aggregate<'_> {
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

/// What a group column is called in this operator's schema.
///
/// A group over a plain column keeps that column's name, because the thing somebody reading a plan
/// dump or a mid-pipeline schema wants to know is which column it is. Anything else gets a
/// positional name, since the projection above an aggregate is what names the query's output and
/// these names never reach a result set.
fn group_name(plan: &Plan, group: ExprRef, input: &Schema, at: usize) -> String {
    if let Expr::Column(binding) = *plan.expr(group) {
        if let Some(position) = input.position_of(binding) {
            return input.fields()[position].name.clone();
        }
    }
    format!("group{at}")
}

/// Duplicate elimination over the whole row or over named expressions.
///
/// `DISTINCT ON (a) b` keeps the first row of each `a`, whole, which is why the kept rows are the
/// input's columns and not the key's. Plain `DISTINCT` is the same operator with the key being
/// every column, and writing it that way rather than as a separate path is what keeps the two from
/// disagreeing about nulls.
#[derive(Debug)]
pub(crate) struct Distinct<'a> {
    input: Box<dyn Operator + 'a>,
    plan: &'a Plan,
    on: Vec<ExprRef>,
    schema: Schema,
    built: bool,
    chunks: Vec<Chunk>,
    at: usize,
}

impl<'a> Distinct<'a> {
    pub(crate) fn new(plan: &'a Plan, input: Box<dyn Operator + 'a>, on: Slice) -> Self {
        let schema = input.schema().clone();
        Self {
            input,
            plan,
            on: plan.expr_list(on).to_vec(),
            schema,
            built: false,
            chunks: Vec::new(),
            at: 0,
        }
    }

    fn build(&mut self) -> Result<()> {
        let mut seen: HashSet<Key> = HashSet::new();
        let mut kept: Vec<Vec<Value>> = Vec::new();
        while let Some(chunk) = self.input.next()? {
            let keys = if self.on.is_empty() {
                Vec::new()
            } else {
                evaluate_all(self.plan, &self.on, &self.schema, &chunk)?
            };
            for row in 0..chunk.len() {
                let values: Vec<Value> = chunk.row(row).collect();
                let key = if self.on.is_empty() {
                    Key(values.clone())
                } else {
                    Key(keys.iter().map(|column| column.value_at(row)).collect())
                };
                if seen.insert(key) {
                    kept.push(values);
                }
            }
        }
        self.chunks = rows::chunks(&self.schema.types(), &kept)?;
        Ok(())
    }
}

impl Operator for Distinct<'_> {
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
