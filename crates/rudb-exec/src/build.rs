//! Turning a bound plan into the pipelines that run it.
//!
//! One match, one arm per logical operator, and nothing else. There is no physical plan and no cost
//! based choice between two ways of running the same node, which is the honest description of tier
//! 0: there is one implementation of each operator so there is nothing to choose between. The
//! physical planner that section 9.6 describes goes here, and the reason this is a separate module
//! from the operators is so that it can grow into one without any of them moving.
//!
//! The pipelines borrow the plan and the catalog for as long as they exist. A scan reads its rows
//! out of the catalog's table rather than copying them, and an expression reads its constants, its
//! function names and its types out of the plan's arena, so a plan that outlives the query it built
//! is the whole of the lifetime story here.
//!
//! # How a tree becomes a list
//!
//! The walk is the same one it always was, down from the root, and what changed is what it carries
//! back up. A node returns a [`Segment`], which is a source with the streaming operators stacked on
//! it so far, and a node that is a pipeline breaker closes the segment under it into a finished
//! [`Pipeline`] and starts a new segment over the buffer that breaker finalises into. So a plan with
//! two breakers in it comes back as three pipelines, and they are pushed onto the list in the order
//! they have to run, because a breaker's own pipeline is closed before the walk returns to whatever
//! is above it.
//!
//! A node with two inputs closes the side that has to finish first and then walks the side that uses
//! it, which is the same order the ids are handed out in and the same order the work happens in.
//!
//! # Where the measurement comes from
//!
//! Every operator this module makes is wrapped in [`Watched`] before it goes into a pipeline, and
//! the counters it reports into are registered with the [`Report`] the caller passed in. That is the
//! only place the wrapping happens, which is what makes it impossible for an operator to be left
//! out: an arm that forgets to wrap is an arm that does not compile, because the id it was handed
//! has to go somewhere.
//!
//! A breaker's counters go around two objects rather than one. The sink is the operator, and the
//! buffer the next pipeline sources from is where its rows come back out, so both are wrapped in the
//! same counters and a sort's row count is the rows it produced rather than zero.
//!
//! Neither the ids nor the pipeline numbers are worked out here. They come from [`Shape`], which is
//! one walk over the plan in `rudb-plan`, because `EXPLAIN` prints the same numbering and the same
//! decomposition without building anything, and two versions of that rule would be right on the day
//! they were written and disagree some time after. What this module does is ask which operator a
//! node is and wrap it.

use std::collections::HashMap;
use std::hash::BuildHasherDefault;
use std::sync::Arc;

use rudb_catalog::{Catalog, Parent, QualifiedName, Table};
use rudb_common::{Cancel, Error, Field, LogicalType, Memory, Result, Rule, Session, Value};
use rudb_functions::TableFunction;
use rudb_graph::Link;
use rudb_kernels::Accumulator;
use rudb_metrics::{Counters, Driver, Report};
use rudb_parquet::{Bound, Op};
use rudb_pipeline::{
    BufferId, DynSink, DynStream, Pipeline, PipelineId, Source, Watched, root, root_in_order,
};
use rudb_plan::{
    BuildSide, ColumnBinding, CompareOp, Expr, ExprRef, JoinKind, Node, NodeRef, PipelineRef, Plan,
    ROOT, Shape, Slice, seams_of,
};
use rudb_seam::Settings;

use crate::buffer::Buffered;
use crate::consistent::{Answer, Collect, Reduction};
use crate::cutoff::{self, Cutoff};
use crate::devicecard::device_card;
use crate::enginenames::{
    database_size, dialects, extensions, grammar_extensions, optimizers, platform, user_agent,
    version,
};
use crate::entrynames::{
    columnnames, constraintnames, databasenames, indexnames, schemanames, sequencenames,
    showdatabases, showtables, showtablesexpanded, tablenames, viewnames,
};
use crate::fetch::{Fetch, TableFetch};
use crate::functionnames::functionnames;
use crate::gather::{Gather, Keep};
use crate::group::{Aggregate, Distinct};
use crate::join::{Broadcast, CrossProduct, Gathered, Join, Marking, Padding, Probe};
use crate::key::Digest;
use crate::keywords::keywords;
use crate::lateral::LateralSeries;
use crate::linkjoin::LinkJoin;
use crate::links::links;
use crate::percent::{LimitPercent, Portion};
use crate::prepared::Prepared;
use crate::query::Query;
use crate::register::registries;
use crate::schema::Schema;
use crate::setop::SetOp;
use crate::settingnames::settingnames;
use crate::sideways::{self, Exact, Keyed, Sideways, Stored};
use crate::sort::Sort;
use crate::source::{
    Dummy, FileScan, Filters, Frequencies, ProjectionDistinct, Pushdown, Scan, Series, Summary,
    Values,
};
use crate::storagenames::storage_info;
use crate::strategies::strategies;
use crate::stream::{Edge, Filter, Limit, Project};
use crate::topn::TopN;
use crate::typenames::typenames;
use crate::unnest::LateralUnnest;
use crate::window::{Window, Written};
use crate::writemetrics::{codec_metrics, statement_metrics, write_metrics};

/// Builds the pipelines for a plan's root, for a query nothing will stop.
///
/// Every seam is left at its default, which is what a caller with no session behind it wants and is
/// what the tests in this crate are written against.
///
/// # Errors
///
/// If the plan names a table or a column the catalog does not have, if an expression is malformed
/// in a way [`Plan::validate`] would have caught, or anything an operator's construction reports.
pub fn build<'a>(plan: &'a Plan, catalog: &'a Catalog) -> Result<Query<'a>> {
    build_with(plan, catalog, &Cancel::new(), &Memory::unlimited(), &Settings::new())
}

/// Builds the pipelines for a plan's root, stoppable through this token and held to this budget.
///
/// The token is checked once per chunk by the driver, so the query stops at the first chunk boundary
/// after the token says to. It is one check in one place rather than a decision per operator,
/// because a decision per operator is a decision somebody gets wrong when they add the twentieth
/// one. What the driver cannot see is work an operator does inside one call, and the join is the one
/// that can: its nested loop runs to the end inside a single push, and a hundred thousand left rows
/// against thirty thousand right ones is a minute with nothing looking at the token, so that loop
/// holds the token as well and checks it once per left row.
///
/// The budget is not uniform, and that is the difference between the two. A streaming operator holds
/// one chunk and gives it away again, so charging every operator would count the same megabyte once
/// per level. Only the operators that buffer without bound take a reservation, and
/// [`rudb_common::Memory`] lists which ones those are.
///
/// The measurement still happens. It goes into a report nobody reads, because the alternative is two
/// builders that drift apart, and a pair of clock readings per chunk is not a cost worth avoiding by
/// having a second one.
///
/// The seam settings are the session's with the statement's hints on top, and they are read here
/// rather than looked up later, because a choice made while the query is built is a choice `EXPLAIN`
/// can print before the query runs. An operator that sits on a seam chooses once, in its
/// constructor, and holds what it chose.
///
/// # Errors
///
/// The same as [`build`].
pub fn build_with<'a>(
    plan: &'a Plan,
    catalog: &'a Catalog,
    cancel: &Cancel,
    memory: &Memory,
    seams: &Settings,
) -> Result<Query<'a>> {
    build_measured(plan, catalog, cancel, memory, seams, &Session::new(), &Report::new())
}

/// Builds the pipelines, reporting what every operator in them did into `report`.
///
/// The report is what the caller keeps. Once the query has been run, [`Report::fill`] turns it into
/// the operator and pipeline rows of a metrics document, and that document is the same one
/// `EXPLAIN ANALYZE` prints and `--metrics` writes.
///
/// The session is what `SET` has left the settings at, and the only thing that reads it is
/// `duckdb_settings()`. It is a separate argument from the seam settings because the seams are a
/// choice an operator makes while it is built and the settings are rows in a table. [`build`] and
/// [`build_with`] pass an empty one, which reports every value as null, since a caller with no
/// database behind it has no settings to report.
///
/// # Errors
///
/// The same as [`build`].
pub fn build_measured<'a>(
    plan: &'a Plan,
    catalog: &'a Catalog,
    cancel: &Cancel,
    memory: &Memory,
    seams: &Settings,
    session: &Session,
    report: &Report,
) -> Result<Query<'a>> {
    build_measured_with_sink(
        plan,
        catalog,
        BuildUnder { cancel, memory, seams, session, report },
        None,
    )
}

/// Builds the pipelines with their root connected to a caller supplied sink.
///
/// This is the write path counterpart of [`build_measured`]. It lets an `INSERT ... SELECT`
/// consume chunks as the producing pipeline runs instead of first collecting the whole result in
/// the root queue.
///
/// # Errors
///
/// The same as [`build_measured`].
pub fn build_measured_into<'a>(
    plan: &'a Plan,
    catalog: &'a Catalog,
    cancel: &Cancel,
    memory: &Memory,
    seams: &Settings,
    session: &Session,
    sink: Arc<dyn DynSink + 'a>,
) -> Result<Query<'a>> {
    let report = Report::new();
    build_measured_with_sink(
        plan,
        catalog,
        BuildUnder { cancel, memory, seams, session, report: &report },
        Some(sink),
    )
}

struct BuildUnder<'a> {
    cancel: &'a Cancel,
    memory: &'a Memory,
    seams: &'a Settings,
    session: &'a Session,
    report: &'a Report,
}

#[derive(Clone, Copy, Default)]
struct AggregateBound {
    max_groups: Option<usize>,
    /// How many groups a count descending TopN above can observe, and which call it ranks them by.
    top_counts: Option<(usize, usize)>,
    having_count: Option<(usize, i64)>,
}

fn build_measured_with_sink<'a>(
    plan: &'a Plan,
    catalog: &'a Catalog,
    under: BuildUnder<'_>,
    sink: Option<Arc<dyn DynSink + 'a>>,
) -> Result<Query<'a>> {
    let BuildUnder { cancel, memory, seams, session, report } = under;
    let shape = Shape::of(plan);
    for pipeline in shape.all() {
        report.pipeline(pipeline);
        for waits_for in shape.waits_for(pipeline) {
            report.depends(pipeline, *waits_for);
        }
    }
    let mut building = Building {
        plan,
        catalog,
        cancel,
        memory,
        seams,
        session,
        report,
        shape,
        done: Vec::new(),
        drivers: Vec::new(),
        pruning: Vec::new(),
        pushing: None,
        sideways: None,
        above: Vec::new(),
        armed: Vec::new(),
        cutoff: None,
        top_counts: Vec::new(),
        marking: None,
        held: Vec::new(),
    };
    let segment = building.node(plan.root())?;
    let schema = segment.schema.clone();
    // A query whose rows come out of a sort or a top n is already in the order somebody asked for,
    // and holding chunks back to restore the source order would only add latency to an order nobody
    // is going to look at. Everything else gets the root that puts them back, because the moment
    // several threads read the same file a plain `SELECT` would otherwise come back in a different
    // order on every run. It costs nothing to decide here and it means the scheduler never has to.
    let reader = if let Some(sink) = sink {
        building.close(segment, ROOT, sink);
        None
    } else {
        let (sink, reader) = if ordered(plan, plan.root()) {
            root(BufferId(0), None)
        } else {
            root_in_order(BufferId(0), None)
        };
        building.close(segment, ROOT, Arc::new(sink));
        Some(reader)
    };
    let Building { done, drivers, .. } = building;
    Query::new(done, drivers, reader, schema)
}

/// Whether the rows reaching the root are already in an order the plan chose.
///
/// A sort and a top n both decide one. Everything between them and the root either keeps the order
/// it was given or is not a node that can sit there, and the walk stops at the first node that is
/// neither.
fn ordered(plan: &Plan, node: NodeRef) -> bool {
    match *plan.node(node) {
        Node::Sort { .. } | Node::TopN { .. } => true,
        Node::Project { input, .. }
        | Node::Filter { input, .. }
        | Node::Limit { input, .. }
        | Node::LimitPercent { input, .. }
        | Node::Fetch { input, .. } => ordered(plan, input),
        _ => false,
    }
}

/// The aggregate under a TopN whose only key is a COUNT result descending, and which call that is.
///
/// Keeping the local prefix from every radix partition is sufficient for the global prefix: a
/// group excluded behind `k` groups in its own partition cannot enter the first `k` overall. The
/// regular TopN remains in the plan and settles the small union, so this only reduces aggregate
/// output and does not replace ordering semantics.
///
/// The call index comes back because an aggregate that is not one of the counting shapes holds one
/// accumulator per call, and the one to rank the groups by is whichever of them the TopN sorts on.
fn count_top_aggregate(plan: &Plan, input: NodeRef, keys: Slice) -> Option<(NodeRef, usize)> {
    let [key] = plan.sort_key_list(keys) else { return None };
    if !key.descending {
        return None;
    }
    let Expr::Column(ordered) = *plan.expr(key.expr) else { return None };
    let mut aggregate = input;
    let mut output = ordered;
    loop {
        match *plan.node(aggregate) {
            Node::Project { input, index, exprs, .. } if output.table == index => {
                let projected = *plan.expr_list(exprs).get(output.column as usize)?;
                let Expr::Column(next) = *plan.expr(projected) else { return None };
                output = next;
                aggregate = input;
            }
            Node::Aggregate { index, .. } if output.table == index => break,
            _ => return None,
        }
    }
    let Node::Aggregate { index, groups, aggregates, .. } = *plan.node(aggregate) else {
        return None;
    };
    if output.table != index {
        return None;
    }
    let call = (output.column as usize).checked_sub(plan.expr_list(groups).len())?;
    let aggregate_call = *plan.expr_list(aggregates).get(call)?;
    let Expr::Aggregate { name, args, distinct, filter } = *plan.expr(aggregate_call) else {
        return None;
    };
    let count_star = plan.string(name) == "count_star"
        && plan.expr_list(args).is_empty()
        && !distinct
        && filter.is_none();
    let distinct_count = plan.string(name) == "count"
        && plan.expr_list(args).len() == 1
        && distinct
        && filter.is_none();
    (count_star || distinct_count).then_some((aggregate, call))
}

/// A direct aggregate under `input` and the COUNT(*) call constrained by a simple lower bound.
///
/// The Filter remains in the pipeline and checks the predicate again. Recognizing only this narrow
/// shape therefore changes how many aggregate rows are materialized and not which rows are valid.
fn count_having_aggregate(
    plan: &Plan,
    input: NodeRef,
    predicate: ExprRef,
) -> Option<(NodeRef, usize, i64)> {
    let Node::Aggregate { index, groups, aggregates, .. } = *plan.node(input) else { return None };
    let Expr::Compare { op, left, right } = *plan.expr(predicate) else { return None };
    let Expr::Column(column) = *plan.expr(left) else { return None };
    let Expr::Constant(value) = *plan.expr(right) else { return None };
    let Value::BigInt(value) = *plan.value(value) else { return None };
    if column.table != index {
        return None;
    }
    let call = (column.column as usize).checked_sub(plan.expr_list(groups).len())?;
    let aggregate = *plan.expr_list(aggregates).get(call)?;
    let Expr::Aggregate { name, args, distinct, filter } = *plan.expr(aggregate) else {
        return None;
    };
    if plan.string(name) != "count_star"
        || !plan.expr_list(args).is_empty()
        || distinct
        || filter.is_some()
    {
        return None;
    }
    let minimum = match op {
        CompareOp::Greater => value.checked_add(1)?,
        CompareOp::GreaterOrEqual => value,
        _ => return None,
    };
    Some((input, call, minimum))
}

fn mark_binding(plan: &Plan, right: NodeRef, kind: JoinKind) -> Option<usize> {
    if kind != JoinKind::Mark {
        return None;
    }
    let Node::Project { exprs, names, .. } = *plan.node(right) else {
        return None;
    };
    let positions: Vec<usize> = plan
        .expr_list(exprs)
        .iter()
        .enumerate()
        .filter_map(|(position, &expr)| {
            let Expr::Constant(value) = *plan.expr(expr) else {
                return None;
            };
            (*plan.value(value) == Value::Boolean(true)).then_some(position)
        })
        .collect();
    let position = match positions.as_slice() {
        [position] => *position,
        _ => plan
            .name_list(names)
            .iter()
            .enumerate()
            .rev()
            .find_map(|(position, &name)| (plan.string(name) == "mark").then_some(position))?,
    };
    Some(position)
}

struct NativePairFrequencies {
    entries: Vec<(Vec<Value>, u64)>,
}

/// The one anchored host expression and aggregate state certified by a native snapshot.
fn native_host_groups(
    plan: &Plan,
    catalog: &Catalog,
    input: NodeRef,
    groups: Slice,
    aggregates: Slice,
    having: Option<(usize, i64)>,
) -> Result<Option<Vec<Vec<Value>>>> {
    let Some((1, minimum)) = having else { return Ok(None) };
    let Ok(minimum) = u64::try_from(minimum) else { return Ok(None) };
    let Node::Filter { input: source, predicate } = *plan.node(input) else {
        return Ok(None);
    };
    let Node::Get { catalog: database, schema, table, index, columns, .. } = *plan.node(source)
    else {
        return Ok(None);
    };
    let [group] = plan.expr_list(groups) else { return Ok(None) };
    let Expr::Function { name, args } = *plan.expr(*group) else { return Ok(None) };
    if plan.string(name) != "regexp_replace" {
        return Ok(None);
    }
    let [subject, pattern, replacement] = plan.expr_list(args) else { return Ok(None) };
    let Expr::Column(binding) = *plan.expr(*subject) else { return Ok(None) };
    if binding.table != index {
        return Ok(None);
    }
    let (Expr::Constant(pattern), Expr::Constant(replacement)) =
        (plan.expr(*pattern), plan.expr(*replacement))
    else {
        return Ok(None);
    };
    if plan.value(*pattern) != &Value::Varchar("^https?://(?:www\\.)?([^/]+)/.*$".into())
        || plan.value(*replacement) != &Value::Varchar("\\1".into())
    {
        return Ok(None);
    }
    let Expr::Compare { op: CompareOp::NotEqual, left, right } = *plan.expr(predicate) else {
        return Ok(None);
    };
    let filtered = match (plan.expr(left), plan.expr(right)) {
        (Expr::Column(held), Expr::Constant(value))
            if plan.value(*value) == &Value::Varchar(String::new()) =>
        {
            held
        }
        (Expr::Constant(value), Expr::Column(held))
            if plan.value(*value) == &Value::Varchar(String::new()) =>
        {
            held
        }
        _ => return Ok(None),
    };
    if filtered != &binding {
        return Ok(None);
    }
    let [average, count, minimum_value] = plan.expr_list(aggregates) else { return Ok(None) };
    let check = |reference: &ExprRef, wanted: &str, argument: Option<ExprRef>| {
        let Expr::Aggregate { name, args, distinct: false, filter: None } = *plan.expr(*reference)
        else {
            return false;
        };
        plan.string(name) == wanted
            && match argument {
                None => plan.expr_list(args).is_empty(),
                Some(argument) => plan.expr_list(args) == [argument],
            }
    };
    if !check(count, "count_star", None) || !check(minimum_value, "min", Some(*subject)) {
        return Ok(None);
    }
    let Expr::Aggregate { name, args, distinct: false, filter: None } = *plan.expr(*average) else {
        return Ok(None);
    };
    if plan.string(name) != "avg" || plan.expr_list(args).len() != 1 {
        return Ok(None);
    }
    let mut length = plan.expr_list(args)[0];
    if let Expr::Cast { input, try_cast: false } = *plan.expr(length) {
        length = input;
    }
    let Expr::Function { name, args } = *plan.expr(length) else { return Ok(None) };
    if plan.string(name) != "strlen" || plan.expr_list(args) != [*subject] {
        return Ok(None);
    }
    let Some(field) = plan.field_list(columns).get(binding.column as usize) else {
        return Ok(None);
    };
    let name = QualifiedName::new(plan.string(database), plan.string(schema), plan.string(table));
    let table = catalog.table(&name)?;
    let Some(column) = table.column_index(&field.name) else { return Ok(None) };
    let Some(entries) = table.rows().host_groups(column, minimum)? else { return Ok(None) };
    let mut records = Vec::with_capacity(entries.len());
    for entry in entries {
        let count = i64::try_from(entry.count)
            .map_err(|_| Error::internal("a stored host count exceeds BIGINT"))?;
        records.push(vec![
            Value::Varchar(entry.host),
            Accumulator::exact_avg(entry.bytes_sum, count, &LogicalType::Double).finish()?,
            Value::BigInt(count),
            Value::Varchar(entry.minimum),
        ]);
    }
    Ok(Some(records))
}

/// Exact two-key counts over bounded heavy-hitter rows, certified against the omitted maximum.
fn native_pair_frequencies(
    plan: &Plan,
    catalog: &Catalog,
    input: NodeRef,
    groups: Slice,
    aggregates: Slice,
    top: usize,
) -> Result<Option<NativePairFrequencies>> {
    let Node::Get { catalog: database, schema, table, index, columns, .. } = *plan.node(input)
    else {
        return Ok(None);
    };
    let [first_expr, second_expr] = plan.expr_list(groups) else { return Ok(None) };
    let Expr::Column(first) = *plan.expr(*first_expr) else { return Ok(None) };
    let Expr::Column(second) = *plan.expr(*second_expr) else { return Ok(None) };
    if first.table != index
        || second.table != index
        || plan.expr_type(*first_expr) != &LogicalType::BigInt
        || plan.expr_type(*second_expr) != &LogicalType::Varchar
    {
        return Ok(None);
    }
    let [aggregate] = plan.expr_list(aggregates) else { return Ok(None) };
    let Expr::Aggregate { name, args, distinct, filter } = *plan.expr(*aggregate) else {
        return Ok(None);
    };
    if top == 0
        || plan.string(name) != "count_star"
        || !plan.expr_list(args).is_empty()
        || distinct
        || filter.is_some()
    {
        return Ok(None);
    }
    let fields = plan.field_list(columns);
    let Some(first_field) = fields.get(first.column as usize) else { return Ok(None) };
    let Some(second_field) = fields.get(second.column as usize) else { return Ok(None) };
    let name = QualifiedName::new(plan.string(database), plan.string(schema), plan.string(table));
    let table = catalog.table(&name)?;
    let Some(first_column) = table.column_index(&first_field.name) else { return Ok(None) };
    let Some(second_column) = table.column_index(&second_field.name) else { return Ok(None) };
    if let Some(entries) = table.rows().top_pair_frequencies(first_column, second_column, top)? {
        return Ok(Some(NativePairFrequencies { entries }));
    }
    let Some(occurrences) = table.rows().frequency_occurrences(first_column)? else {
        return Ok(None);
    };
    let (anchors, anchor_indices, second_values, dictionary) = if !occurrences
        .anchor_indices
        .is_empty()
        && occurrences.anchor_indices.len() == occurrences.ordinals.len()
    {
        let Some((second_values, dictionary)) =
            table.rows().stable_codes_at(second_column, &occurrences.ordinals)?
        else {
            return Ok(None);
        };
        if occurrences.anchors.iter().any(|value| !matches!(value, Value::BigInt(_) | Value::Null))
            || occurrences
                .anchor_indices
                .iter()
                .any(|&entry| entry as usize >= occurrences.anchors.len())
        {
            return Err(Error::internal("a BIGINT frequency anchor has another type"));
        }
        (occurrences.anchors, occurrences.anchor_indices, second_values, dictionary)
    } else {
        let Some(rows) = table.rows().stable_pair_codes_at(
            first_column,
            second_column,
            &occurrences.ordinals,
        )?
        else {
            return Ok(None);
        };
        let mut anchors = Vec::new();
        let mut by_anchor = HashMap::<Option<i64>, u16, BuildHasherDefault<Digest>>::default();
        let mut anchor_indices = Vec::with_capacity(rows.first.len());
        for value in rows.first {
            let value = value
                .map(|value| {
                    i64::try_from(value).map_err(|_| {
                        Error::internal("a BIGINT frequency occurrence is out of range")
                    })
                })
                .transpose()?;
            let entry = match by_anchor.get(&value) {
                Some(&entry) => entry,
                None => {
                    let entry = u16::try_from(anchors.len())
                        .map_err(|_| Error::internal("too many frequency anchors"))?;
                    anchors.push(value.map_or(Value::Null, Value::BigInt));
                    by_anchor.insert(value, entry);
                    entry
                }
            };
            anchor_indices.push(entry);
        }
        (anchors, anchor_indices, rows.second, rows.dictionary)
    };
    if anchor_indices.len() != second_values.len() {
        return Err(Error::internal("a stable pair fetch returned columns of different lengths"));
    }
    let mut counts = HashMap::<(u16, Option<u32>), u64, BuildHasherDefault<Digest>>::default();
    for (anchor, second) in anchor_indices.into_iter().zip(second_values) {
        *counts.entry((anchor, second)).or_default() += 1;
    }
    let mut boundaries = counts.values().copied().collect::<Vec<_>>();
    if boundaries.len() < top {
        return Ok(None);
    }
    boundaries.select_nth_unstable_by(top - 1, |left, right| right.cmp(left));
    let boundary = boundaries[top - 1];
    if boundary <= occurrences.omitted_max {
        return Ok(None);
    }
    let mut entries = Vec::new();
    for ((anchor, second), count) in counts {
        if count < boundary {
            continue;
        }
        let first = anchors
            .get(anchor as usize)
            .cloned()
            .ok_or_else(|| Error::internal("a frequency anchor index is outside its values"))?;
        let second = match second {
            Some(code) => Value::Varchar(
                dictionary
                    .try_text_at(code as usize)?
                    .ok_or_else(|| Error::internal("a string frequency code is null"))?
                    .to_owned(),
            ),
            None => Value::Null,
        };
        entries.push((vec![first, second], count));
    }
    Ok(Some(NativePairFrequencies { entries }))
}

/// One end of a limit, ready to run.
///
/// A number the binder worked out comes over as it is. One it could not is an expression over the
/// limit's own input, because the binder put the value in a column of every row that reaches here,
/// and it is compiled now so that the first chunk has nothing to do but evaluate it.
fn edge(plan: &Plan, bound: rudb_plan::Bound, input: &Schema) -> Result<Edge> {
    Ok(match bound {
        rudb_plan::Bound::All => Edge::All,
        rudb_plan::Bound::Rows(rows) => Edge::Rows(rows),
        rudb_plan::Bound::Read(expr) => Edge::Read(Box::new(Prepared::one(plan, expr, input)?)),
    })
}

/// The share of a limit written as a percentage, ready to run, and the same rule as [`edge`].
fn portion(plan: &Plan, share: rudb_plan::Share, input: &Schema) -> Result<Portion> {
    Ok(match share {
        rudb_plan::Share::Percent(percent) => Portion::Percent(percent),
        rudb_plan::Share::Read(expr) => Portion::Read(Box::new(Prepared::one(plan, expr, input)?)),
    })
}

/// The stored table one node reads straight through, with no filter and nothing else in the way.
///
/// Everything below answers questions about a whole table, so a node that drops rows or invents
/// them has to stop the search here. A `Get` is the only node that reads a table and changes
/// nothing about it.
///
/// Either kind of table, because both keep statistics now. A file has a directory per stripe and a
/// table in memory has a zone map per chunk and a sketch and a tally per column, and the questions
/// below are asked of `Rows` rather than of one or the other, so a table that cannot answer one of
/// them says so and the caller goes and reads the rows.
fn whole_table<'a>(
    plan: &Plan,
    catalog: &'a Catalog,
    node: NodeRef,
) -> Result<Option<(&'a Table, u32, Slice)>> {
    let Node::Get { catalog: database, schema, table, index, columns, .. } = *plan.node(node)
    else {
        return Ok(None);
    };
    let name = QualifiedName::new(plan.string(database), plan.string(schema), plan.string(table));
    let table = catalog.table(&name)?;
    Ok(Some((table, index, columns)))
}

/// The stored table one node under `node` reads, found by the table index its columns are bound to.
///
/// A link join's child is a scan and may be a scan under a filter, because a row id survives a
/// filter as the selection is applied to the sequence that carries it. So the search is down the
/// tree rather than at the top of it, and what it matches on is the index the join's own condition
/// named, which is the only thing that says which scan is the one the link is indexed by.
fn scanned<'a>(
    plan: &Plan,
    catalog: &'a Catalog,
    node: NodeRef,
    index: u32,
) -> Result<Option<(&'a Table, Slice)>> {
    if let Some((table, found, columns)) = whole_table(plan, catalog, node)?
        && found == index
    {
        return Ok(Some((table, columns)));
    }
    for child in plan.node(node).children().into_iter().flatten() {
        if let Some(found) = scanned(plan, catalog, child, index)? {
            return Ok(Some(found));
        }
    }
    Ok(None)
}

/// What turns a join's build side into exact driving rows, when the join is over a stored link, or
/// into an exact test of the driving column when the parent has a key map and there is no link.
///
/// spec/graph/05-execution.md section 5.4, and the conditions that make it exact rather than
/// approximately right. The build side's key has to be the parent's stored key column, read as it
/// is, so that every key on that side is a key the parent's key map holds. It can come up through
/// joins as well as filters and projections, which is the shape of Q3 and Q5 where `orders` is
/// joined to a filtered `customer` before `lineitem` is joined to it, because a key the map does not
/// hold sends the join back to the filter and so the walk only has to find which table the values
/// were read from, not prove that every one of them is still a row of it. The driving column has
/// to be the child's stored column the link was built over, so that the link says which parent
/// every driving row's value names. And both tables have to be one committed file each, so that a
/// row's position in the scan is its `rid`. The join kind and the null rule were settled by the
/// operator before this was asked, the same as for the filter.
///
/// `None` on anything else, which is a join that gets the Bloom filter it always got. A catalog
/// error is `None` as well rather than a failed query, because the only thing lost is a faster path.
fn exact(
    plan: &Plan,
    catalog: &Catalog,
    parent: NodeRef,
    key: ExprRef,
    driving: NodeRef,
    binding: ColumnBinding,
) -> Option<Exact> {
    let Expr::Column(key) = *plan.expr(key) else { return None };
    let (child_table, child_columns) = scanned(plan, catalog, driving, binding.table).ok()??;
    let child_column = stored_column(plan, child_table, binding.table, child_columns, binding)?;
    let keyed = traced(plan, parent, key).and_then(|key| {
        let (parent_table, parent_columns) = scanned(plan, catalog, parent, key.table).ok()??;
        let parent_rows = parent_table.rows().stored()?;
        let parent_column = stored_column(plan, parent_table, key.table, parent_columns, key)?;
        rudb_native::graph::holds_key_map(parent_rows, parent_column)
            .then_some((parent_table, parent_column))
    });
    let (parent_table, parent_column) =
        keyed.or_else(|| linked_parent(catalog, child_table, child_column))?;
    let parent_rows = parent_table.rows().stored()?;
    // No link is a join that still has the key map, and the key map alone is enough for an exact
    // test of the driving column's values, see `sideways::Domain`. A link over the budget is not in
    // the file, and neither is one for a child that is not one committed file. Both are read when
    // the build side first asks, see `sideways::Exact`.
    let child = child_table.rows().stored().map(|child_rows| {
        let edge = rudb_native::graph::Edge {
            child: child_table.name().table.clone(),
            child_column,
            parent: parent_table.name().table.clone(),
            parent_column,
        };
        (child_rows.clone(), edge)
    });
    Some(Exact::stored(Stored { parent: parent_rows.clone(), column: parent_column, child }))
}

/// The parent the driving column's stored link points into, when that parent has a key map over
/// the column the link was built against.
///
/// This is how a build side whose key is not a parent's key column gets an exact set all the same.
/// On TPC-H q21 the build side is lines of `lineitem` and the driving side is `lineitem` again,
/// joined on `l_orderkey`. Neither side is `orders`, but every value either side holds is an order
/// key, and the driving column's link says which order each driving row names. So the build side's
/// keys go through the key map of `orders` to a set of orders and that set goes through the link to
/// the driving rows, the same as when the build side is `orders` itself. A build key that is not a
/// key of the parent is not in the key map, and the lookup that finds that out sends the join back
/// to the filter, see `sideways::held_parents`, so nothing here has to prove that the build side's
/// values are parent keys.
fn linked_parent<'a>(
    catalog: &'a Catalog,
    child: &Table,
    child_column: usize,
) -> Option<(&'a Table, usize)> {
    let (name, column) = rudb_native::graph::link_parent(child.rows().stored()?, child_column)?;
    let owner = child.name();
    let parent = catalog
        .table(&QualifiedName::new(owner.catalog.clone(), owner.schema.clone(), name))
        .ok()?;
    rudb_native::graph::holds_key_map(parent.rows().stored()?, column).then_some((parent, column))
}

/// The stored column under `node` that `binding` reads, through anything that passes it along.
///
/// A projection renames, so the binding becomes the column in its position, and an expression there
/// ends the walk because the values it makes are not a column's. Every other node either passes its
/// children's bindings through unchanged or makes a table of its own, and a binding into a table a
/// node made is found under none of its children, since a table index names one node in a plan. So
/// the walk goes into whichever child the binding turns up in and stops at the scan that made it.
fn traced(plan: &Plan, node: NodeRef, binding: ColumnBinding) -> Option<ColumnBinding> {
    match *plan.node(node) {
        Node::Get { index, .. } => (binding.table == index).then_some(binding),
        Node::Project { input, index, exprs, .. } => {
            let binding = if binding.table == index {
                let at = *plan.expr_list(exprs).get(binding.column as usize)?;
                let Expr::Column(inner) = *plan.expr(at) else { return None };
                inner
            } else {
                binding
            };
            traced(plan, input, binding)
        }
        _ => plan
            .node(node)
            .children()
            .into_iter()
            .flatten()
            .find_map(|child| traced(plan, child, binding)),
    }
}

/// The two columns one condition holds equal, when it is an equality between two columns.
///
/// Anything else is `None`, including one equality between a column and something computed, because
/// a link is indexed by a column and an expression over one is not that column.
fn equated(plan: &Plan, condition: ExprRef) -> Option<[ColumnBinding; 2]> {
    let Expr::Compare { op: CompareOp::Equal, left, right } = *plan.expr(condition) else {
        return None;
    };
    match (plan.expr(left), plan.expr(right)) {
        (&Expr::Column(left), &Expr::Column(right)) => Some([left, right]),
        _ => None,
    }
}

/// Which column of the stored table a binding into `index` names, by name rather than by position.
fn stored_column(
    plan: &Plan,
    table: &Table,
    index: u32,
    columns: Slice,
    binding: ColumnBinding,
) -> Option<usize> {
    if binding.table != index {
        return None;
    }
    let field = plan.field_list(columns).get(binding.column as usize)?;
    table.column_index(&field.name)
}

/// The stored column a grouping with no aggregates puts a whole table into groups by.
///
/// This is the shape `COUNT(DISTINCT column)` is planned as: one grouping that throws the rows away
/// and keeps the distinct values, with a count of those values over it. The one column it produces
/// is the group, so a binding into it names position zero and nothing else.
fn grouped_column<'a>(
    plan: &Plan,
    catalog: &'a Catalog,
    node: NodeRef,
) -> Result<Option<(&'a Table, u32, usize)>> {
    let Node::Aggregate { input, index: produced, groups, aggregates } = *plan.node(node) else {
        return Ok(None);
    };
    if !plan.expr_list(aggregates).is_empty() {
        return Ok(None);
    }
    let [group] = plan.expr_list(groups) else { return Ok(None) };
    let Expr::Column(binding) = *plan.expr(*group) else { return Ok(None) };
    let Some((table, index, columns)) = whole_table(plan, catalog, input)? else {
        return Ok(None);
    };
    Ok(stored_column(plan, table, index, columns, binding).map(|column| (table, produced, column)))
}

/// A filter over a stored table whose rows the file can count without reading any of them.
///
/// The shape is one equality or inequality against a constant, over a column the file wrote a
/// frequency synopsis for. That synopsis is the leading distinct values of the column with an exact
/// count each and a bound on every value it left out, so which values the predicate keeps and how
/// many rows hold them are both already known for the values it lists, and the whole of
/// `WHERE AdvEngineID <> 0` is a walk over fourteen entries rather than a million.
///
/// A complete synopsis can give the row count for a filter. Grouped output still reads rows.
struct CertainFilter {
    /// The leading values of the column the predicate names, with their exact row counts.
    entries: Vec<(Value, u64)>,
    /// How many rows any value outside `entries` can hold, and zero when there are none.
    omitted_max: u64,
    /// The constant the predicate compares against, never null.
    against: Value,
    /// Whether the predicate keeps the rows that differ rather than the ones that match.
    differs: bool,
}

impl CertainFilter {
    /// How many rows the predicate keeps, or `None` if any entry cannot be decided.
    ///
    /// Needs the complete list. A value the synopsis left out is a value whose rows are missing
    /// from this sum, and a row count that is quietly short is worse than no row count at all.
    fn rows(&self) -> Option<u64> {
        if self.omitted_max != 0 {
            return None;
        }
        let mut kept = 0_u64;
        for (value, count) in &self.entries {
            if self.keeps(value)? {
                kept = kept.checked_add(*count)?;
            }
        }
        Some(kept)
    }

    /// Whether the predicate keeps the rows holding one value.
    ///
    /// `Value`'s `PartialEq` is Rust equality rather than SQL equality, and it says so, so leaning on
    /// it here needs an argument. The two places they differ are nulls, which it calls equal and SQL
    /// calls unknown, and NaNs, which it calls equal and SQL does not. Neither can arrive: a null is
    /// answered above without being compared, a null constant is turned away when the filter is
    /// recognised, and a float column never has a synopsis at all. What is left is integers, dates,
    /// timestamps and strings of one declared type, and for those two the two equalities are the same
    /// relation.
    fn keeps(&self, value: &Value) -> Option<bool> {
        // A null row answers unknown to both comparisons and a filter keeps neither, which is the one
        // thing a count over the entries would get wrong if it just compared.
        if value.is_null() {
            return Some(false);
        }
        if value.logical_type() != self.against.logical_type() {
            return None;
        }
        Some((value == &self.against) != self.differs)
    }
}

/// The filter above a stored table that [`CertainFilter`] can answer, if this node is one.
fn certain_filter(plan: &Plan, catalog: &Catalog, node: NodeRef) -> Result<Option<CertainFilter>> {
    let Node::Filter { input, predicate } = *plan.node(node) else { return Ok(None) };
    let Some((table, index, columns)) = whole_table(plan, catalog, input)? else {
        return Ok(None);
    };
    let Expr::Compare { op, left, right } = *plan.expr(predicate) else { return Ok(None) };
    let differs = match op {
        CompareOp::Equal => false,
        CompareOp::NotEqual => true,
        _ => return Ok(None),
    };
    // Written either way round is the same question, since neither side depends on the other.
    let (binding, constant) = match (plan.expr(left), plan.expr(right)) {
        (&Expr::Column(binding), &Expr::Constant(value))
        | (&Expr::Constant(value), &Expr::Column(binding)) => (binding, value),
        _ => return Ok(None),
    };
    let against = plan.value(constant).clone();
    // A null constant makes the comparison unknown for every row whatever the column holds, so the
    // answer is no rows and it is not worth a shape of its own. The operator says so already.
    if against.is_null() {
        return Ok(None);
    }
    let Some(column) = stored_column(plan, table, index, columns, binding) else {
        return Ok(None);
    };
    let Some(prefix) = table.rows().frequency_prefix(column)? else { return Ok(None) };
    let (entries, omitted_max) = (prefix.entries, prefix.omitted_max);
    Ok(Some(CertainFilter { entries, omitted_max, against, differs }))
}

/// How many rows a node produces, when that can be known without producing them.
///
/// A `Get` knows because the file wrote down its row count. A grouping with no aggregates knows when
/// the file knows how many distinct values the grouping column has, which for a string column of
/// this format it does exactly, because the dictionary holds every distinct value once and holds
/// nothing else. That is the difference between reading the answer and building a hash table with a
/// hundred thousand rows in it.
///
/// A filter knows when it is one comparison against a constant over a column with a complete
/// frequency synopsis, because then the file already holds how many rows every value has and the
/// predicate only has to pick which of them count.
///
/// `None` means go and count them.
fn known_rows(plan: &Plan, catalog: &Catalog, node: NodeRef) -> Result<Option<u64>> {
    if let Some((table, _, _)) = whole_table(plan, catalog, node)? {
        return Ok(Some(table.rows().len() as u64));
    }
    if let Some(filter) = certain_filter(plan, catalog, node)? {
        return Ok(filter.rows());
    }
    let Some((table, _, column)) = grouped_column(plan, catalog, node)? else { return Ok(None) };
    let Some(distinct) = table.rows().distinct_values(column)? else { return Ok(None) };
    // A grouping puts every null in a group of its own and a distinct count does not count it, so a
    // column with a null in it has one group more than it has distinct values. Both numbers are
    // exact, so adding them is exact too, and a file that answers the second answers the first.
    let Some(nulls) = table.rows().null_count(column)? else { return Ok(None) };
    Ok(Some(distinct.saturating_add(u64::from(nulls > 0))))
}

/// Every aggregate of a whole table aggregation, answered from the statistics of the table.
///
/// `None` the moment one of them cannot be, because a query that reads the rows for one aggregate
/// may as well read them for all of them. What is answerable here is deliberately small and exact.
/// A count is a number the table wrote down, a distinct count is the size of a dictionary that holds
/// every distinct value once, the extremes of a string column are the two ends of the order written
/// beside that dictionary, and the extremes and the total of an integer column are the per chunk or
/// per stripe ranges added up. None of these is a sketch and none of them is a bound that is allowed
/// to be wide, so none of them can be off by one.
///
/// A table in memory answers the counts, the extremes and the total out of the zone map it builds as
/// each chunk arrives, and a native file answers all of those and the two that need a persisted
/// synopsis. Neither path decides anything here: the question goes to `Rows` and a table that cannot
/// answer it says so, which is what sends the query off to read the rows.
///
/// The one thing this does not do is decide differently from the operators. A sum and an average
/// finish through the state a grouped aggregation would have built, so the rounding, the overflow
/// and the answer for a column with no rows in it are the operator's rather than a second opinion.
fn stored_summary(
    plan: &Plan,
    catalog: &Catalog,
    input: NodeRef,
    groups: Slice,
    aggregates: Slice,
) -> Result<Option<Vec<Value>>> {
    if !plan.expr_list(groups).is_empty() || plan.expr_list(aggregates).is_empty() {
        return Ok(None);
    }
    let below = whole_table(plan, catalog, input)?;
    let mut values = Vec::with_capacity(plan.expr_list(aggregates).len());
    for &aggregate in plan.expr_list(aggregates) {
        let Expr::Aggregate { name, args, distinct, filter: None } = *plan.expr(aggregate) else {
            return Ok(None);
        };
        let call = plan.string(name);
        let args = plan.expr_list(args);
        // `COUNT(DISTINCT c)` that the distinct rewrite left where it was, which is the one column
        // `BIGINT` shape `rudb-opt`'s `already_cheap` hands to the operator's inline integer state
        // rather than staging into a grouping. It never becomes the grouping the arm further down
        // reads, so without this the column type most likely to be a key is the one type a whole
        // table distinct count cannot be answered for.
        if distinct {
            if call != "count" {
                return Ok(None);
            }
            let [only] = args else { return Ok(None) };
            let Expr::Column(binding) = *plan.expr(*only) else { return Ok(None) };
            let Some((table, index, columns)) = below else { return Ok(None) };
            let Some(column) = stored_column(plan, table, index, columns, binding) else {
                return Ok(None);
            };
            let Some(counted) = table.rows().distinct_values(column)? else { return Ok(None) };
            values.push(count(counted)?);
            continue;
        }
        if call == "count_star" && args.is_empty() {
            let Some(rows) = known_rows(plan, catalog, input)? else { return Ok(None) };
            values.push(count(rows)?);
            continue;
        }
        let [only] = args else { return Ok(None) };
        let Expr::Column(binding) = *plan.expr(*only) else { return Ok(None) };
        // Counting the one column a grouping produced is counting its distinct values, which is the
        // other half of how `COUNT(DISTINCT column)` is planned. The count drops the null group and
        // the distinct count never had it, so the two agree.
        if call == "count"
            && let Some((table, produced, column)) = grouped_column(plan, catalog, input)?
            && binding.table == produced
            && binding.column == 0
        {
            let Some(distinct) = table.rows().distinct_values(column)? else {
                return Ok(None);
            };
            values.push(count(distinct)?);
            continue;
        }
        let Some((table, index, columns)) = below else { return Ok(None) };
        let Some(column) = stored_column(plan, table, index, columns, binding) else {
            return Ok(None);
        };
        match call {
            // Counting a column is counting the rows that are not null, and both of those numbers
            // are written down.
            "count" => {
                let Some(nulls) = table.rows().null_count(column)? else { return Ok(None) };
                values.push(count(table.rows().len() as u64 - nulls)?);
            }
            "min" | "max" => {
                let Some(value) = extreme(table, column, call == "min")? else { return Ok(None) };
                values.push(value);
            }
            // Adding a column up is adding its stripe totals up, and dividing that by the rows that
            // went into it is the average. Both finish through the same state the operator would
            // have built, so a file that answers this cannot answer it differently.
            "sum" | "avg" => {
                let Some(field) = table.columns().get(column) else { return Ok(None) };
                if !field.ty.is_integer() {
                    return Ok(None);
                }
                let Some((total, rows)) = table.rows().exact_sum(column)? else { return Ok(None) };
                let returns = plan.expr_type(aggregate);
                let state = if call == "sum" {
                    Accumulator::exact_sum(total, rows > 0, returns)
                } else {
                    let Ok(seen) = i64::try_from(rows) else { return Ok(None) };
                    Accumulator::exact_avg(total, seen, returns)
                };
                values.push(state.finish()?);
            }
            _ => return Ok(None),
        }
    }
    Ok(Some(values))
}

/// What an ungrouped aggregation produces, worked out from the plan rather than from its input.
///
/// With no groups the output is one field per aggregate, named after the call and typed by what the
/// binder decided it returns, and none of that depends on the rows underneath. That is what lets a
/// summary answer without building the operators below it, which is the whole point: an input that
/// gets built also gets run.
fn summary_schema(plan: &Plan, index: u32, aggregates: Slice) -> Result<Schema> {
    let mut fields = Vec::with_capacity(plan.expr_list(aggregates).len());
    for &reference in plan.expr_list(aggregates) {
        let Expr::Aggregate { name, .. } = *plan.expr(reference) else {
            return Err(Error::internal("an aggregate list holds something that is not a call"));
        };
        fields.push(Field::new(plan.string(name).to_string(), plan.expr_type(reference).clone()));
    }
    Ok(Schema::numbered(fields, index))
}

/// A native row-preserving projection usable for a bound grouped distinct aggregate.
///
/// The lookup uses column bindings and the current table generation. SQL spelling, aliases,
/// ordering, and limits do not enter the decision. Unsupported plans keep the regular aggregate.
fn covering_grouped_distinct<'a>(
    plan: &Plan,
    catalog: &'a Catalog,
    input: NodeRef,
    groups: Slice,
    aggregates: Slice,
) -> Result<Option<(&'a rudb_native::Reader, usize, usize, LogicalType)>> {
    let Some((table, index, columns)) = whole_table(plan, catalog, input)? else {
        return Ok(None);
    };
    let [group] = plan.expr_list(groups) else { return Ok(None) };
    let Expr::Column(group_binding) = *plan.expr(*group) else { return Ok(None) };
    let [aggregate] = plan.expr_list(aggregates) else { return Ok(None) };
    let Expr::Aggregate { name, args, distinct: true, filter: None } = *plan.expr(*aggregate)
    else {
        return Ok(None);
    };
    if plan.string(name) != "count" {
        return Ok(None);
    }
    let [argument] = plan.expr_list(args) else { return Ok(None) };
    let Expr::Column(order_binding) = *plan.expr(*argument) else { return Ok(None) };
    let (Some(group_column), Some(order_column)) = (
        stored_column(plan, table, index, columns, group_binding),
        stored_column(plan, table, index, columns, order_binding),
    ) else {
        return Ok(None);
    };
    let group_type = plan.expr_type(*group).clone();
    if !matches!(group_type, LogicalType::TinyInt | LogicalType::SmallInt | LogicalType::Integer) {
        return Ok(None);
    }
    let Some(reader) = table.rows().stored() else { return Ok(None) };
    if !reader.has_run_projection(order_column, group_column)? {
        return Ok(None);
    }
    Ok(Some((reader, order_column, group_column, group_type)))
}

/// One end of a stored column, from the two places a file keeps one.
///
/// The dictionary is asked first, because a string column keeps its values in sorted order and the
/// two ends of that order are the answer with nothing to walk and nothing to convert. Everything
/// else comes from the stripe ranges, which are only an answer when every stripe wrote ends it had
/// really looked at rather than ends it was allowed to widen.
fn extreme(table: &Table, column: usize, smallest: bool) -> Result<Option<Value>> {
    if let Some((low, high)) = table.rows().text_extremes(column)? {
        return Ok(Some(if smallest { low } else { high }));
    }
    let Some((low, high)) = table.rows().exact_extremes(column)? else { return Ok(None) };
    let Some(field) = table.columns().get(column) else { return Ok(None) };
    Ok(if smallest { low } else { high }.into_value(&field.ty))
}

/// A row count as the BIGINT every count aggregate produces.
fn count(rows: u64) -> Result<Value> {
    Ok(Value::BigInt(
        i64::try_from(rows).map_err(|_| Error::internal("a stored row count exceeds BIGINT"))?,
    ))
}

/// A pipeline being built from the bottom up.
///
/// It is not a [`Pipeline`] yet because it has no sink. What ends it is whichever node above it
/// turns out to be a pipeline breaker, or the root of the plan, and neither is known until the walk
/// gets back there.
struct Segment<'a> {
    source: Arc<dyn Source + 'a>,
    /// In the order they run, nearest the source first.
    streams: Vec<Arc<dyn DynStream + 'a>>,
    /// What the segment produces as it stands, which changes as streams are added.
    schema: Schema,
    /// The pipelines this one cannot start before.
    after: Vec<PipelineRef>,
}

impl<'a> Segment<'a> {
    /// A segment that is just its source.
    fn new(source: Arc<dyn Source + 'a>, schema: Schema) -> Self {
        Self { source, streams: Vec::new(), schema, after: Vec::new() }
    }

    /// A segment reading what a pipeline breaker finalised into.
    fn reading(source: Arc<dyn Source + 'a>, schema: Schema, after: PipelineRef) -> Self {
        Self { source, streams: Vec::new(), schema, after: vec![after] }
    }

    /// Puts a streaming operator on the end, which becomes what the segment produces.
    fn then(mut self, stream: Arc<dyn DynStream + 'a>, schema: Schema) -> Self {
        self.streams.push(stream);
        self.schema = schema;
        self
    }
}

/// What the walk down the plan carries with it.
struct Building<'a, 'b> {
    plan: &'a Plan,
    catalog: &'a Catalog,
    cancel: &'b Cancel,
    memory: &'b Memory,
    seams: &'b Settings,
    /// What `SET` has left the settings at, which only `duckdb_settings()` reads.
    session: &'b Session,
    report: &'b Report,
    shape: Shape,
    /// The pipelines closed so far, in the order they have to run.
    done: Vec<Pipeline<'a>>,
    /// One per entry of `done`, in the same order.
    drivers: Vec<Arc<Driver>>,
    /// The bounds tests the filter arm worked out for the scan it is about to walk into.
    ///
    /// A scan is built before the filter above it, because the filter needs the schema the scan
    /// produces, so by the time there is a filter to read there is already a scan that cannot be
    /// told anything. This carries the tests the other way, down the one step from a filter to its
    /// own input, and the scan arm takes them. It is empty every other time it is read, and empty
    /// means hand out every row group, which is what every scan did before pruning existed.
    pruning: Vec<(usize, Op, Bound)>,
    /// The whole filter, offered to the scan directly below it to apply rather than only to prune
    /// with.
    ///
    /// Travels the same one step down as `pruning` and answers a different question. Pruning is
    /// always worth handing over, because a test the scan cannot use costs nothing. This is only
    /// handed over when the scan can apply all of it, since a scan applying some of a filter and no
    /// filter running above it is rows that should have gone and did not, so the filter arm offers it
    /// and then reads whether it was taken. Taken is the scan arm leaving `None` here, and that is
    /// the arm's way of saying the operator above it is not needed.
    pushing: Option<Pushdown>,
    /// The runtime filter of the join whose driving side is being walked into, for the scan at the
    /// bottom of it.
    ///
    /// The same one step down that `pruning` is, except that it survives more than one step: a scan
    /// under a filter under a join is the shape this is worth the most on. What it does not survive
    /// is anything that decides which rows come out by counting rather than by value, because a scan
    /// that drops rows under a `LIMIT` changes which rows reach the limit. [`Builder::node`] clears
    /// it for every node that is not a scan, a filter or a projection.
    sideways: Option<Arc<Sideways<'a>>>,
    /// The runtime filters of joins further up, for the same scan.
    ///
    /// A join's own filter stops at the next join down, and these are the ones that went on through
    /// it because that join lets a driving row's columns through unchanged. See
    /// [`sideways::through`] for which joins do. Cleared by every node that `sideways` is cleared
    /// by and that is not such a join.
    above: Vec<Arc<Sideways<'a>>>,
    /// Every join's runtime filter, in the order the joins were built.
    ///
    /// A join reads the ones its driving side added to narrow its own table. See
    /// [`crate::join::Probe::narrowed_by`].
    armed: Vec<Arc<Sideways<'a>>>,
    /// The cutoff of the TopN whose input is being walked into, for the scan at the bottom of it.
    ///
    /// The same walk `sideways` survives, minus the table function: a file scan prunes by row group
    /// of somebody else's file rather than by part of ours, so there is nothing down there for a
    /// cutoff to be measured against yet. See [`crate::cutoff`].
    cutoff: Option<Arc<Cutoff>>,
    /// Aggregates whose parent TopN orders by COUNT descending, its count plus offset, and which
    /// call of the aggregate that count is.
    top_counts: Vec<(NodeRef, usize, usize)>,
    /// The filter directly under an aggregate that reads a marked chunk, for the filter arm to tell
    /// the scan or the filter operator to mark the rows it keeps rather than cut them. See
    /// [`marks_through`].
    marking: Option<NodeRef>,
    /// The materialisations whose bodies are being walked, innermost last.
    held: Vec<Held>,
}

/// What a link join reads out of the catalog, gathered before either input is built.
struct Linked {
    link: Arc<Link>,
    parent: Arc<Parent>,
    /// The stored position and the type of each parent column the join projects, in output order.
    projected: Vec<(usize, LogicalType)>,
    /// Those same columns as the schema the bindings above this join resolve against.
    parent_schema: Schema,
    /// The equalities the join is on, each as the child's column and the parent's.
    keys: Vec<(ColumnBinding, ColumnBinding)>,
}

/// A materialised `WITH` that has been built, for the reads of it under the body being walked.
struct Held {
    /// The number the plan pairs a read with what it reads by.
    cte: u32,
    /// What the definition filled, which every read takes a reader of its own on.
    chunks: Buffered,
    /// The pipeline that fills it, which every pipeline a read is in has to wait for.
    filling: PipelineRef,
}

/// Whether this statement asked for a CPU column on every operator's row.
///
/// It is off by default and that is a performance decision rather than a policy one. The number
/// comes from `CLOCK_THREAD_CPUTIME_ID`, which has no vDSO entry on Linux, so reading it is a real
/// system call, and the shim reads it twice per operator per chunk. On chunks of a thousand rows
/// that came to more than the operators themselves: a count over twenty million rows was fourteen
/// times slower with the reading than without it. Wall time per operator stays on, because the wall
/// clock does come out of the vDSO, and CPU time per pipeline and per worker stays on because those
/// spans are taken once per pipeline rather than once per chunk.
///
/// The two ways to ask are `EXPLAIN ANALYZE`, which sets this for its own run, and
/// `PRAGMA enable_profiling`, which sets it for every statement after it until
/// `PRAGMA disable_profiling`. The second writes the sentinel the settings layer uses for a null
/// back, which is why the value is compared against it rather than merely being present.
fn profiling(session: &Session) -> bool {
    session.get("enable_profiling").is_some_and(|format| format != rudb_functions::UNSET)
}

/// The one table name a pragma was called with.
///
/// The binder folds the two column describing pragmas into a `VALUES` while it binds them, so their
/// name never reaches here. `pragma_storage_info` is the one that does, because its rows are read
/// off the file and there are as many of them as the table has parts times columns, which is not
/// something to fold into a plan.
///
/// A name that is not a constant is refused rather than evaluated, which is the same answer the
/// binder gives the other two and for the same missing piece: there is no constant folding in front
/// of this, so `pragma_storage_info('l' || 'ineitem')` is an expression at this point and not a
/// name.
fn pragma_name(plan: &Plan, args: Slice) -> Result<String> {
    let [argument] = plan.expr_list(args) else {
        return Err(Error::internal("a pragma that resolved to more than one name"));
    };
    let Expr::Constant(reference) = *plan.expr(*argument) else {
        return Err(Error::not_implemented(
            "pragma_storage_info() given a name that is not a constant",
        ));
    };
    match plan.value(reference) {
        Value::Varchar(name) => Ok(name.clone()),
        Value::Null => Ok("NULL".to_string()),
        other => Err(Error::internal(format!("a pragma name bound as VARCHAR arrived as {other}"))),
    }
}

impl<'a> Building<'a, '_> {
    /// The id of the operator holding the side of this node that has to finish first.
    ///
    /// # Panics
    ///
    /// If the node has one input, which is a node whose arm below should not have called this.
    fn gathered(&self, node: NodeRef) -> u32 {
        self.shape.gathered(node).expect("a node with two inputs has a second operator")
    }

    /// Ends a segment with a sink and puts the finished pipeline on the list.
    fn close(&mut self, segment: Segment<'a>, id: PipelineRef, sink: Arc<dyn DynSink + 'a>) {
        let mut pipeline = Pipeline::new(PipelineId(id), segment.source, sink);
        for stream in segment.streams {
            pipeline = pipeline.then(stream);
        }
        for after in segment.after {
            pipeline = pipeline.after(PipelineId(after));
        }
        self.done.push(pipeline);
        self.drivers.push(self.report.driving(id));
    }

    /// The counters for one operator, registered with the report.
    ///
    /// The row records what this operator picked at each seam it sits on, which is `seams_of` on
    /// its plan node crossed with what is registered and what the statement pinned. That is the
    /// same three things `EXPLAIN` puts its reference marker from, and it is read here rather than
    /// asserted here for a reason worth writing down: this used to mark every operator as a
    /// reference implementation unconditionally, so every ClickBench run said 41 of 41 operators
    /// ran the slow path no matter what had actually run, and the fold that reported it was read as
    /// if it meant something.
    ///
    /// An operator that sits on no registered seam records nothing and stays marked as a reference,
    /// because there is one implementation of it and that one is the obvious correct one. The
    /// marker comes off by itself on the day a seam under it has something else registered and
    /// chosen, with nothing to remember to change here.
    ///
    /// This is also where the shim around the operator is told whether to read the thread clock, for
    /// the reason [`profiling`] gives. It is decided here because this is the one place that can see
    /// the session the statement is running under, and once per operator rather than once per chunk.
    fn watch(
        &self,
        node: NodeRef,
        id: u32,
        pipeline: u32,
        kind: &str,
        detail: Option<&str>,
    ) -> Arc<Counters> {
        self.watch_doing(node, None, id, pipeline, kind, detail)
    }

    /// The same, for an operator that is also doing the work of a node that got no operator.
    ///
    /// A scan that took a filter into itself sits on that filter's seams as well as its own, and it
    /// is the only row in the document where they can be reported, since the filter has no row. Left
    /// out, pinning a compaction strategy and then reading the document to see whether it ran gives
    /// the answer no on every query whose filter moved down, which is most of ClickBench.
    ///
    /// The two nodes are different kinds, so their seam lists do not overlap and neither entry is
    /// written twice. Two nodes of a kind would be a different arrangement than this one.
    fn watch_doing(
        &self,
        node: NodeRef,
        also: Option<NodeRef>,
        id: u32,
        pipeline: u32,
        kind: &str,
        detail: Option<&str>,
    ) -> Arc<Counters> {
        let mut counters = Counters::new(id, pipeline, kind)
            .charging_cpu(profiling(self.session))
            .under(self.consumer(id, also));
        if let Some(detail) = detail {
            counters = counters.detailed(detail);
        }
        for node in std::iter::once(node).chain(also) {
            for seam in seams_of(self.plan.node(node)) {
                if let Some(running) = registries().running(*seam, self.seams) {
                    counters = counters.chose(seam.name(), &running.name, running.is_reference);
                }
            }
        }
        self.report.watch(counters)
    }

    /// Which operator's row this one's rows go into, skipping any node it swallowed.
    ///
    /// A node folded into another gets no row of its own, so a scan that took a filter into itself
    /// is the only row either of them has and it produces what the filter would have, to whoever
    /// was reading the filter. Naming the filter would leave a parent id pointing at nothing, and a
    /// reader checking an operator's input against what fed it would find a gap where the chain
    /// should be.
    fn consumer(&self, id: u32, also: Option<NodeRef>) -> Option<u32> {
        let swallowed = also.map(|node| self.shape.operator(node));
        let mut parent = self.shape.consumer(id);
        while parent.is_some() && parent == swallowed {
            parent = self.shape.consumer(parent?);
        }
        parent
    }

    /// Every call that stands where a table goes, which is a file scan, a metadata table or a
    /// series.
    ///
    /// Its own function for the reason [`Self::join`] is: [`Self::node`] recurses once per plan
    /// node and a debug frame carries every local of every arm. This arm is a match of its own
    /// with a couple of dozen branches under it, none of which can recurse, so it is pure weight
    /// on a frame that every deep plan pays for.
    ///
    /// It reads the node again rather than being handed the six fields, because six more
    /// arguments is a signature nobody can call correctly and the read is a slice index.
    fn table_function(&mut self, reference: NodeRef) -> Result<Segment<'a>> {
        let plan = self.plan;
        let id = self.shape.operator(reference);
        let pipeline = self.shape.pipeline(reference);
        let Node::TableFunction { index, function, args, options, settings, columns } =
            *plan.node(reference)
        else {
            return Err(Error::internal("a table function was built from a node that is not one"));
        };
        let name = plan.string(function);
        // Taken here rather than inside the file scan arm so that a table function that is
        // not one leaves nothing behind for whatever is built next.
        let runtime = self.sideways.take();
        self.above.clear();
        Ok(match TableFunction::lookup(name) {
            Some(function @ (TableFunction::ReadParquet | TableFunction::ReadCsv)) => {
                let counters = self.watch(reference, id, pipeline, "FileScan", Some(name));
                let tests = std::mem::take(&mut self.pruning);
                let scan = FileScan::new(
                    plan, index, function, args, options, settings, columns, tests, runtime,
                )?
                .watched(counters.clone());
                let schema = scan.schema().clone();
                Segment::new(Arc::new(Watched::new(scan, counters)), schema)
            }
            // A call where a table goes with nothing to its left is the lateral one over the
            // single row of a `FROM` with nothing in it, which is what its arguments read.
            Some(TableFunction::Unnest) => {
                let dummy = Dummy::new();
                let below = dummy.schema().clone();
                let unnest = LateralUnnest::new(plan, &below, index, args, columns, self.cancel)?
                    .in_session(self.session);
                let schema = unnest.schema().clone();
                let counters = self.watch(reference, id, pipeline, "Unnest", None);
                Segment::new(Arc::new(dummy), below)
                    .then(Arc::new(Watched::new(unnest, counters)), schema)
            }
            Some(TableFunction::PragmaStorageInfo) => {
                let written = pragma_name(plan, args)?;
                let table = storage_info(self.catalog, &written, plan, index, columns)?;
                let schema = table.schema().clone();
                let counters = self.watch(
                    reference,
                    id,
                    pipeline,
                    "Metadata",
                    Some(TableFunction::PragmaStorageInfo.name()),
                );
                Segment::new(Arc::new(Watched::new(table, counters)), schema)
            }
            Some(TableFunction::RudbDeviceCard) => {
                let table = device_card(plan, args, index, columns)?;
                let schema = table.schema().clone();
                let counters = self.watch(
                    reference,
                    id,
                    pipeline,
                    "Metadata",
                    Some(TableFunction::RudbDeviceCard.name()),
                );
                Segment::new(Arc::new(Watched::new(table, counters)), schema)
            }
            Some(
                function @ (TableFunction::RudbStrategies
                | TableFunction::RudbLinks
                | TableFunction::RudbWriteMetrics
                | TableFunction::RudbCodecMetrics
                | TableFunction::RudbStatementMetrics
                | TableFunction::DuckdbKeywords
                | TableFunction::DuckdbTypes
                | TableFunction::DuckdbFunctions
                | TableFunction::DuckdbSettings
                | TableFunction::DuckdbDatabases
                | TableFunction::DuckdbSchemas
                | TableFunction::DuckdbTables
                | TableFunction::DuckdbViews
                | TableFunction::DuckdbSequences
                | TableFunction::DuckdbIndexes
                | TableFunction::DuckdbConstraints
                | TableFunction::DuckdbColumns
                | TableFunction::DuckdbExtensions
                | TableFunction::DuckdbOptimizers
                | TableFunction::DuckdbDialects
                | TableFunction::DuckdbGrammarExtensions
                | TableFunction::PragmaVersion
                | TableFunction::PragmaPlatform
                | TableFunction::PragmaUserAgent
                | TableFunction::PragmaDatabaseSize
                | TableFunction::PragmaShowTables
                | TableFunction::PragmaShowDatabases
                | TableFunction::PragmaShowTablesExpanded),
            ) => {
                let table = match function {
                    TableFunction::RudbLinks => {
                        links(self.session, self.catalog, plan, index, columns)?
                    }
                    TableFunction::RudbWriteMetrics => write_metrics(plan, index, columns)?,
                    TableFunction::RudbCodecMetrics => codec_metrics(plan, index, columns)?,
                    TableFunction::RudbStatementMetrics => statement_metrics(plan, index, columns)?,
                    TableFunction::DuckdbKeywords => keywords(plan, index, columns)?,
                    TableFunction::DuckdbTypes => typenames(self.catalog, plan, index, columns)?,
                    TableFunction::DuckdbFunctions => functionnames(plan, index, columns)?,
                    TableFunction::DuckdbSettings => {
                        settingnames(self.session, plan, index, columns)?
                    }
                    TableFunction::DuckdbDatabases => {
                        databasenames(self.catalog, plan, index, columns)?
                    }
                    TableFunction::DuckdbSchemas => {
                        schemanames(self.catalog, plan, index, columns)?
                    }
                    TableFunction::DuckdbTables => tablenames(self.catalog, plan, index, columns)?,
                    TableFunction::DuckdbViews => viewnames(self.catalog, plan, index, columns)?,
                    TableFunction::DuckdbSequences => {
                        sequencenames(self.catalog, plan, index, columns)?
                    }
                    TableFunction::DuckdbIndexes => indexnames(self.catalog, plan, index, columns)?,
                    TableFunction::DuckdbConstraints => {
                        constraintnames(self.catalog, plan, index, columns)?
                    }
                    TableFunction::DuckdbColumns => {
                        columnnames(self.catalog, plan, index, columns)?
                    }
                    TableFunction::DuckdbExtensions => extensions(plan, index, columns)?,
                    TableFunction::DuckdbOptimizers => optimizers(plan, index, columns)?,
                    TableFunction::DuckdbDialects => dialects(plan, index, columns)?,
                    TableFunction::DuckdbGrammarExtensions => {
                        grammar_extensions(plan, index, columns)?
                    }
                    TableFunction::PragmaVersion => version(plan, index, columns)?,
                    TableFunction::PragmaPlatform => platform(plan, index, columns)?,
                    TableFunction::PragmaUserAgent => user_agent(plan, index, columns)?,
                    TableFunction::PragmaDatabaseSize => {
                        database_size(self.catalog, self.memory, plan, index, columns)?
                    }
                    TableFunction::PragmaShowTables => {
                        showtables(self.catalog, plan, index, columns)?
                    }
                    TableFunction::PragmaShowDatabases => {
                        showdatabases(self.catalog, plan, index, columns)?
                    }
                    TableFunction::PragmaShowTablesExpanded => {
                        showtablesexpanded(self.catalog, plan, index, columns)?
                    }
                    _ => strategies(plan, index, columns)?,
                };
                let schema = table.schema().clone();
                // `EXPLAIN` names the table rather than the operator, because every one of
                // these is the same operator and a plan that said `Metadata` four times
                // would not say which four tables it read.
                let counters =
                    self.watch(reference, id, pipeline, "Metadata", Some(function.name()));
                Segment::new(Arc::new(Watched::new(table, counters)), schema)
            }
            _ => {
                let series = Series::new(plan, index, name, args)?;
                let schema = series.schema().clone();
                let counters = self.watch(reference, id, pipeline, "Series", Some(name));
                Segment::new(Arc::new(Watched::new(series, counters)), schema)
            }
        })
    }

    /// The hash join, and the three operators that are it done better.
    ///
    /// Its own function rather than an arm of [`Self::node`] because [`Self::node`] recurses once
    /// per plan node and a debug frame carries every local of every arm it might take. This arm
    /// holds four operators by value before any of them is behind an `Arc`, which made it the
    /// largest of them by some way, and a plan deep enough to matter ran out of stack on the
    /// platform with the smallest one before it ran out of nodes. Moving it here costs a call and
    /// buys back the depth for every plan that is not a join.
    ///
    /// It reads the node again rather than being handed the five fields, for the reason
    /// [`Self::table_function`] does.
    fn join(&mut self, reference: NodeRef) -> Result<Segment<'a>> {
        let plan = self.plan;
        let memory = self.memory;
        let id = self.shape.operator(reference);
        let pipeline = self.shape.pipeline(reference);
        let Node::Join { left, right, kind, conditions, build } = *plan.node(reference) else {
            return Err(Error::internal("a join was built from a node that is not one"));
        };
        // One side runs first, because no row of the other one can be answered until every
        // row it might match has been seen. That is the dependency edge, and it is the same
        // one the hash join builds on. The driving side is a pipeline of its own rather than
        // part of the one above it, because it ends in a sink, and it waits for the build
        // side.
        //
        // Which side is which is the flag, written by `rudb_opt`'s `sides` pass from an
        // estimate of how many rows each input produces. Running the two the other way
        // round means running the mirror of the join kind, because a kind names its sides:
        // a `LEFT` join with its inputs swapped is a `RIGHT` join over the same rows. The
        // pass only ever sets the flag on the kinds that have a mirror, and this refuses
        // the rest rather than producing the wrong answer quietly.
        let marker = mark_binding(plan, right, kind);
        let swapped = build == BuildSide::Left;
        let (held, driving) = if swapped { (left, right) } else { (right, left) };
        // A semi or an anti join has no mirror, because the kind it would be mirrored into
        // is not a kind: its left input is the subject rather than a side and swapping the
        // two does not give a join anybody can write down. What it gives is a different
        // operator over the same join, one that gathers the subject and marks it as the
        // other side streams past, and `crate::join::Marking` is that operator. The kind
        // stays the plan's own, so the swap costs no new spelling of anything either.
        let marking = swapped && matches!(kind, JoinKind::Semi | JoinKind::Anti);
        let kind = if swapped && !marking {
            kind.mirrored().ok_or_else(|| {
                Error::internal(format!(
                    "a {} join was given a build side it has no mirror for",
                    kind.keyword()
                ))
            })?
        } else {
            kind
        };
        let gather_id = self.gathered(reference);
        let gathering = self.shape.pipeline(held);
        let parent = held;
        // Not for the gathered side, which is read once per match and whose rows say nothing about
        // which driving rows the joins above will drop.
        let above = std::mem::take(&mut self.above);
        let held = self.node(held)?;
        self.above = above;
        let held_schema = held.schema.clone();
        // The edge this join's runtime filter crosses, made before either side is built
        // because the sink on one side fills it and the scan on the other reads it. It stays
        // inert unless the join arms it below, which most joins cannot. See
        // `crate::sideways`.
        let sideways = Sideways::new();
        // The chunks as chunks rather than a row per row. A join reads this side by
        // position, to build its table and then once per match, so taking it apart into a
        // `Vec<Value>` per row here would be an allocation per row for a layout the join
        // then has to transpose back into columns. See `crate::side::Build`.
        // A positional join pairs row `n` of one side with row `n` of the other, so for that
        // one the order this side is kept in is the answer and the pipeline under it runs
        // on one thread. Every other kind reads this side through a table or by position
        // and the order only decides which of two equal rows comes out first.
        let ordered = kind == JoinKind::Positional;
        let (gather, gathered) = Keep::watching(memory, Some(Arc::clone(&sideways)), ordered);
        let watched = self.watch(reference, gather_id, gathering, "Gather", None);
        self.close(held, gathering, Arc::new(Watched::new(gather, watched)));
        // Offered to the driving side while it is built, which is how it reaches the scan
        // down there. Cleared afterwards so that nothing built later picks it up.
        self.sideways = Some(Arc::clone(&sideways));
        let from = self.armed.len();
        let mut left = self.node(driving)?;
        self.sideways = None;
        self.above.clear();
        // The filters of the joins on the driving side, which are the only ones this join may use
        // on its own table, and then its own for the joins above it.
        let below: Vec<Arc<Sideways<'a>>> = self.armed[from..].to_vec();
        self.armed.push(Arc::clone(&sideways));
        let side = Gathered { schema: &held_schema, chunks: gathered, marker, swapped };
        // A lookup answers this join and the kind decides about a driving row from that
        // row's own matches, so nothing has to be held and the driving side streams
        // through. That is one less copy of a side, an answer that is never collected, and
        // a pipeline no longer pinned to one thread by a sink that refuses to run twice.
        //
        // The plan's shape does not know about this and counts a pipeline here that the
        // built query then fuses away, the same way it would if a cross product were a join
        // node. Nothing runs wrong because of it: what the driver waits on is `after` on
        // the segment, which is set right below, and the shape is only where the numbers on
        // the counters come from. What it costs is that a profile divides the time between
        // two pipeline ids that are really one, and what it would take to fix is teaching
        // `rudb_plan` the same question this line asks, in a second place, where the two
        // could disagree and the disagreement would be a wrong plan rather than a coarse
        // profile.
        // Armed now rather than when the filter was made, because whether there is a key
        // to hand over is a question about the conditions and only the operator has split
        // them. A join that answers nothing leaves the filter inert, which is a scan that
        // reads everything exactly as it did before.
        // The binding the join knows is the one the projection above the scan hands it, so
        // it is turned into the scan's own before either half is armed. Both halves
        // together, because the build side pass that fills the filter is only worth making
        // when there is a scan that will read it.
        let zone = self.session.session_time_zone();
        let (catalog, reducing) =
            (self.catalog, self.session.rules().enabled(Rule::GraphReduction));
        let arm = |keyed: Vec<(ExprRef, ColumnBinding)>| {
            let Some((key, binding)) = keyed.into_iter().find_map(|(key, binding)| {
                sideways::beneath(plan, driving, binding).map(|binding| (key, binding))
            }) else {
                return;
            };
            if reducing && let Some(exact) = exact(plan, catalog, parent, key, driving, binding) {
                sideways.exactly(exact);
            }
            sideways.keying(Keyed::new(plan, key, held_schema.clone(), zone));
            sideways.about(binding);
        };
        // The join turned around, which is the subject side gathered and marked while the
        // other side streams past. It ends the pipeline rather than sitting in it, because
        // no gathered row can be said to have matched nothing until the last driving row
        // has been through. See `crate::join::Marking`.
        if marking {
            let made =
                Marking::new(plan, &left.schema, &side, kind, conditions, self.cancel, memory);
            let Some((mark, out)) = made else {
                return Err(Error::internal(format!(
                    "a {} join was given a build side no lookup can answer it from",
                    kind.keyword()
                )));
            };
            let mark = mark.in_session(self.session);
            arm(mark.sideways());
            let schema = mark.schema().clone();
            let counters = self.watch(reference, id, pipeline, "Mark", None);
            let reading = Arc::clone(&counters);
            let mark = mark.watched(Arc::clone(&counters));
            left.after.push(gathering);
            self.close(left, pipeline, Arc::new(Watched::new(mark, counters)));
            let reader = Arc::new(Watched::new(out, reading));
            return Ok(Segment::reading(reader, schema, pipeline));
        }
        // An outer join that gathered the side it keeps. It streams the pairs like any
        // probe and owes a padded row for every gathered row nothing matched, which it
        // hands over once the driving side is finished. See `crate::join::Padding`.
        if let Some(pad) =
            Padding::new(plan, &left.schema, &side, kind, conditions, self.cancel, memory)
        {
            let pad = pad.in_session(self.session);
            arm(pad.sideways());
            let schema = pad.schema().clone();
            // Named for what it does rather than for the kind, because the kind on the
            // plan line above it already says which outer join this is and what a reader
            // of a profile wants to know here is which of the two operators ran.
            let counters = self.watch(reference, id, pipeline, "Pad", None);
            let pad = pad.watched(Arc::clone(&counters));
            left.after.push(gathering);
            return Ok(left.then(Arc::new(Watched::new(pad, counters)), schema));
        }
        // A scalar subquery with nothing to correlate on. The gathered row goes beside every
        // driving chunk, in the driving pipeline, rather than through the nested loop below,
        // which gathers the driving side as rows on one thread. See `crate::join::Broadcast`.
        if kind == JoinKind::Single && conditions.is_empty() && !swapped {
            let broadcast = Broadcast::new(&left.schema, side.schema, side.chunks.clone());
            let schema = broadcast.schema().clone();
            let counters = self.watch(reference, id, pipeline, "Broadcast", None);
            left.after.push(gathering);
            return Ok(left.then(Arc::new(Watched::new(broadcast, counters)), schema));
        }
        if let Some(probe) =
            Probe::new(plan, &left.schema, &side, kind, conditions, self.cancel, memory)
        {
            let probe = probe.in_session(self.session);
            arm(probe.sideways());
            // A filter below is about the scan's column, so this join's driving column is named
            // the same way before the two are compared.
            let narrowing = probe
                .driving_columns()
                .into_iter()
                .filter_map(|(at, binding)| {
                    let binding = sideways::beneath(plan, driving, binding)?;
                    let found = below.iter().find(|below| below.binding() == Some(binding))?;
                    found.wanted();
                    Some((at, Arc::clone(found)))
                })
                .collect();
            let probe = probe.narrowed_by(narrowing);
            let schema = probe.schema().clone();
            let counters = self.watch(reference, id, pipeline, "Probe", None);
            let probe = probe.watched(Arc::clone(&counters));
            left.after.push(gathering);
            return Ok(left.then(Arc::new(Watched::new(probe, counters)), schema));
        }
        let (join, out) =
            Join::new(plan, &left.schema, side, kind, conditions, self.cancel, memory);
        let join = join.in_session(self.session);
        let schema = join.schema().clone();
        let counters = self.watch(reference, id, pipeline, "Join", None);
        let reading = Arc::clone(&counters);
        let join = join.watched(Arc::clone(&counters));
        left.after.push(gathering);
        self.close(left, pipeline, Arc::new(Watched::new(join, counters)));
        Ok(Segment::reading(Arc::new(Watched::new(out, reading)), schema, pipeline))
    }

    fn aggregate(
        &mut self,
        reference: NodeRef,
        input: NodeRef,
        index: u32,
        groups: Slice,
        aggregates: Slice,
        bound: AggregateBound,
    ) -> Result<Segment<'a>> {
        if bound.max_groups.is_none()
            && bound.having_count.is_none()
            && let Some((reader, order, covered, group_type)) =
                covering_grouped_distinct(self.plan, self.catalog, input, groups, aggregates)?
        {
            let schema = Schema::numbered(
                vec![
                    Field::new("group".to_string(), group_type),
                    Field::new("count".to_string(), LogicalType::BigInt),
                ],
                index,
            );
            let source = ProjectionDistinct::new(reader, order, covered, schema.clone());
            let id = self.shape.operator(reference);
            let pipeline = self.shape.pipeline(reference);
            let counters =
                self.watch(reference, id, pipeline, "Aggregate", Some("covering grouped distinct"));
            return Ok(Segment::new(Arc::new(Watched::new(source, counters)), schema));
        }
        // Before the input is built, because building it is what puts it in a pipeline and a
        // pipeline that exists is a pipeline that runs. A summary that let the rows be counted
        // underneath it would answer in no time and take exactly as long as it always did.
        // `SET stored_answers = off` skips this and reads the rows, which is how a ClickBench run keeps what the loader added up out of its numbers.
        let stored = self.session.rules().enabled(Rule::StoredAnswers);
        if stored
            && bound.max_groups.is_none()
            && bound.having_count.is_none()
            && let Some(values) =
                stored_summary(self.plan, self.catalog, input, groups, aggregates)?
        {
            let schema = summary_schema(self.plan, index, aggregates)?;
            let source = Summary::new(&schema, &values)?;
            let id = self.shape.operator(reference);
            let pipeline = self.shape.pipeline(reference);
            let counters = self.watch(reference, id, pipeline, "Aggregate", Some("stored summary"));
            return Ok(Segment::new(Arc::new(Watched::new(source, counters)), schema));
        }
        self.marking = marks_through(self.plan, input, groups, aggregates).then_some(input);
        let below = self.node(input);
        self.marking = None;
        let below = below?;
        let (aggregate, out) =
            Aggregate::new(self.plan, &below.schema, index, groups, aggregates, self.memory)?;
        let aggregate = aggregate.in_session(self.session);
        let aggregate = match bound.max_groups {
            Some(limit) => aggregate.limit_groups(limit),
            None => aggregate,
        };
        // Never more room than the cap a pushed down limit already put on the groups. A table that
        // is going to stop at ten groups and took room for a million would be holding it for groups
        // the operator above is about to refuse to open.
        // Not for an aggregate that closes its groups, whose tables only ever see the first and last
        // run of each chunk, so room for every group would be room taken per instance for nothing.
        let aggregate = match self.plan.presized(index).filter(|_| !self.plan.clustered(index)) {
            Some(groups) => aggregate.presize(match bound.max_groups {
                Some(limit) => groups.min(u64::try_from(limit).unwrap_or(u64::MAX)),
                None => groups,
            }),
            None => aggregate,
        };
        let aggregate = if self.session.rules().enabled(Rule::MemoryReservation) {
            aggregate.reserved()
        } else {
            aggregate
        };
        // The range of the one integer grouping key, where the planner found one. Not capped by the
        // group limit the way the presize above is, because this is the range the key lies in and
        // not a number of groups to take room for: narrowing it would leave values with no cell,
        // which is exactly what it is not allowed to be.
        let aggregate = match self.plan.dense(index) {
            Some((low, values)) => aggregate.over_range(low, values),
            None => aggregate,
        };
        // The ends of a key too sparse for that, which its map of values starts from.
        let aggregate = match self.plan.key_ends(index) {
            Some((low, values)) => aggregate.within(low, values),
            None => aggregate,
        };
        // Whether the key arrives in ascending order, where the planner could say so.
        let aggregate = if self.plan.grouped(index) {
            aggregate.grouped()
        } else if self.plan.clustered(index) {
            aggregate.clustered()
        } else {
            aggregate
        };
        let aggregate = match bound.top_counts {
            Some((bound, call)) => aggregate.top_counts(bound, call),
            None => aggregate,
        };
        let aggregate = match bound.having_count {
            Some((call, minimum)) => aggregate.having_count(call, minimum),
            None => aggregate,
        };
        let schema = aggregate.schema().clone();
        let id = self.shape.operator(reference);
        let pipeline = self.shape.pipeline(reference);
        if bound.max_groups.is_none()
            && let Some(records) = native_host_groups(
                self.plan,
                self.catalog,
                input,
                groups,
                aggregates,
                bound.having_count,
            )?
        {
            let source = Frequencies::records(schema.clone(), records)?;
            let counters =
                self.watch(reference, id, pipeline, "Aggregate", Some("native host groups"));
            return Ok(Segment::new(Arc::new(Watched::new(source, counters)), schema));
        }
        if bound.max_groups.is_none() && bound.having_count.is_none() {
            let top = bound.top_counts.map(|(bound, _)| bound);
            if let Some(top) = top
                && let Some(frequencies) = native_pair_frequencies(
                    self.plan,
                    self.catalog,
                    input,
                    groups,
                    aggregates,
                    top,
                )?
            {
                let source = Frequencies::grouped(schema.clone(), frequencies.entries)?;
                let counters = self.watch(
                    reference,
                    id,
                    pipeline,
                    "Aggregate",
                    Some("native pair frequencies"),
                );
                return Ok(Segment::new(Arc::new(Watched::new(source, counters)), schema));
            }
        }
        let counters = self.watch(reference, id, pipeline, "Aggregate", None);
        let reading = Arc::clone(&counters);
        self.close(below, pipeline, Arc::new(Watched::new(aggregate, counters)));
        Ok(Segment::reading(Arc::new(Watched::new(out, reading)), schema, pipeline))
    }

    /// Everything a link join needs out of the catalog, or the reason it cannot be built.
    ///
    /// The edge is derived here rather than carried in the plan, and the derivation is the reason
    /// `Plan::check` insists on one equality per key column. Each equality names two columns, each
    /// of them a binding into a table index, and a table index reaches the `Get` that introduced
    /// it, and a `Get` names a stored table whose columns have positions. So the four fields of the
    /// [`rudb_native::graph::Edge`] a stored link is looked up by are read out of the plan rather
    /// than written into it, and there is no second spelling of the relationship that could
    /// disagree with the first.
    ///
    /// Every failure here is internal, because the rule that wrote the node is the one that checked
    /// all of this. The checks stay anyway: what they defend against is a link written against a
    /// table that has since been rewritten, and section 3.1 says a graph section may only change
    /// the time.
    fn linked(
        &self,
        reference: NodeRef,
        child: NodeRef,
        parent: NodeRef,
        conditions: Slice,
    ) -> Result<Linked> {
        let (plan, catalog) = (self.plan, self.catalog);
        let refuse =
            |why: &str| Error::internal(format!("a link join over {why}, which cannot be read"));
        let Some((parent_table, parent_index, parent_columns)) =
            whole_table(plan, catalog, parent)?
        else {
            return Err(refuse("a parent that is not a stored table"));
        };
        let keys = plan
            .expr_list(conditions)
            .iter()
            .map(|&condition| equated(plan, condition))
            .collect::<Option<Vec<_>>>()
            .ok_or_else(|| refuse("something other than equalities between two columns"))?;
        // Either way round is the same edge, since an equality has no sides.
        let mut oriented = Vec::with_capacity(keys.len());
        for [first, second] in keys {
            oriented.push(match (first.table == parent_index, second.table == parent_index) {
                (false, true) => (first, second),
                (true, false) => (second, first),
                _ => return Err(refuse("an equality that does not read both of its inputs")),
            });
        }
        let Some(&(child_key, _)) = oriented.first() else {
            return Err(refuse("no equality at all"));
        };
        if oriented.iter().any(|(key, _)| key.table != child_key.table) {
            return Err(refuse("a key whose child columns come from two tables"));
        }
        let Some((child_table, child_columns)) = scanned(plan, catalog, child, child_key.table)?
        else {
            return Err(refuse("a child that is not a stored table"));
        };
        let (Some(child_rows), Some(parent_rows)) =
            (child_table.rows().stored(), parent_table.rows().stored())
        else {
            return Err(refuse("a table that is not one committed file"));
        };
        // The key numbers the file names a relationship by, one column or a pair of them, in the
        // order of the equalities. The rule accepted either order, and the link was stored under
        // the declared one, so the pair is tried both ways round before it is refused.
        let mut child_at = Vec::with_capacity(oriented.len());
        let mut parent_at = Vec::with_capacity(oriented.len());
        for &(child_key, parent_key) in &oriented {
            child_at.push(
                stored_column(plan, child_table, child_key.table, child_columns, child_key)
                    .ok_or_else(|| refuse("a child key that is not a stored column"))?,
            );
            parent_at.push(
                stored_column(plan, parent_table, parent_index, parent_columns, parent_key)
                    .ok_or_else(|| refuse("a parent key that is not a stored column"))?,
            );
        }
        let edge = |child_at: &[usize], parent_at: &[usize]| {
            Some(rudb_native::graph::Edge {
                child: child_table.name().table.clone(),
                child_column: rudb_native::graph::key_of(child_at)?,
                parent: parent_table.name().table.clone(),
                parent_column: rudb_native::graph::key_of(parent_at)?,
            })
        };
        let mut edges = vec![edge(&child_at, &parent_at)];
        child_at.reverse();
        parent_at.reverse();
        if child_at.len() == 2 {
            edges.push(edge(&child_at, &parent_at));
        }
        let link = edges
            .into_iter()
            .flatten()
            .find_map(|edge| rudb_native::graph::stored_link(child_rows, parent_rows, &edge))
            .ok_or_else(|| refuse("a relationship the child's file has no link for"))?;
        // A semi or an anti join reads no column of the parent, which is not a special case here so
        // much as the reason those two are nearly free: the list below is empty, so the operator
        // holds nothing, reads nothing, and its output is the child's columns as they arrived.
        let reads = !matches!(
            *plan.node(reference),
            Node::LinkJoin { kind: JoinKind::Semi | JoinKind::Anti, .. }
        );
        let fields = if reads { plan.field_list(parent_columns).to_vec() } else { Vec::new() };
        let mut projected = Vec::with_capacity(fields.len());
        for field in &fields {
            let at = parent_table
                .column_index(&field.name)
                .ok_or_else(|| refuse("a parent column the stored table does not have"))?;
            projected.push((at, field.ty.clone()));
        }
        // What is left of the query's budget, since the columns are held for as long as anything
        // above can still read through them. A parent that will not fit in it is reported by the
        // operator rather than here, as the parts it needs are read.
        let budget = self.memory.limit().map_or(usize::MAX, |limit| {
            usize::try_from(limit.saturating_sub(self.memory.used())).unwrap_or(usize::MAX)
        });
        Ok(Linked {
            link: Arc::new(link),
            parent: Arc::new(Parent::new(parent_table.rows().clone(), budget)),
            projected,
            parent_schema: Schema::numbered(fields, parent_index),
            keys: oriented,
        })
    }

    /// The segment a node produces, closing any pipeline that ends underneath it.
    fn node(&mut self, reference: NodeRef) -> Result<Segment<'a>> {
        let plan = self.plan;
        let memory = self.memory;
        let id = self.shape.operator(reference);
        let pipeline = self.shape.pipeline(reference);
        // A runtime filter reaches a scan through a filter and a projection and through nothing
        // else, because everything else either rebinds the column it is about or decides which rows
        // come out by counting them. See `Builder::sideways`. A table function is a scan for this
        // purpose when it reads a file, and the branch below takes the filter whether it is one or
        // not, so a table function that is not a file scan drops it here all the same.
        if !matches!(
            *plan.node(reference),
            Node::Get { .. }
                | Node::Filter { .. }
                | Node::Project { .. }
                | Node::TableFunction { .. }
        ) {
            // A join that passes a driving row through unchanged passes the filters about it on
            // too, and they go into `above` so that its own can take the place of the one it got.
            let inherited = self.sideways.take();
            if sideways::through(plan.node(reference)).is_some() {
                self.above.extend(inherited);
            } else {
                self.above.clear();
            }
        }
        // A cutoff travels the same way and stops one node short of it, for the reason on the field.
        if !matches!(
            *plan.node(reference),
            Node::Get { .. } | Node::Filter { .. } | Node::Project { .. }
        ) {
            self.cutoff = None;
        }
        let segment = match *plan.node(reference) {
            Node::Get { catalog: database, schema, table, index, columns, .. } => {
                let name = QualifiedName::new(
                    plan.string(database),
                    plan.string(schema),
                    plan.string(table),
                );
                for aside in &self.above {
                    aside.set_aside();
                }
                let filters = Filters {
                    pruning: std::mem::take(&mut self.pruning),
                    pushed: self.pushing.take(),
                    sideways: self.sideways.take(),
                    also: std::mem::take(&mut self.above),
                    cutoff: self.cutoff.take(),
                };
                // Read before the filters are handed over, because it is the one thing the scan's
                // row in the document needs out of them and the scan owns them after this line.
                let moved = filters.pushed.as_ref().map(|pushed| pushed.node);
                let counters = self.watch_doing(
                    reference,
                    moved,
                    id,
                    pipeline,
                    "Scan",
                    Some(plan.string(table)),
                );
                let scan = Scan::new(
                    plan,
                    self.catalog.table(&name)?,
                    index,
                    columns,
                    filters,
                    self.seams,
                    self.session,
                )?
                .watched(counters.clone());
                let schema = scan.schema().clone();
                Segment::new(Arc::new(Watched::new(scan, counters)), schema)
            }
            Node::Dummy => {
                let dummy = Dummy::new();
                let schema = dummy.schema().clone();
                let counters = self.watch(reference, id, pipeline, "Dummy", None);
                Segment::new(Arc::new(Watched::new(dummy, counters)), schema)
            }
            Node::Values { index, columns, rows } => {
                let values = Values::new(plan, index, columns, rows, self.session)?;
                let schema = values.schema().clone();
                let counters = self.watch(reference, id, pipeline, "Values", None);
                Segment::new(Arc::new(Watched::new(values, counters)), schema)
            }
            Node::TableFunction { .. } => self.table_function(reference)?,
            Node::LateralFunction { input, index, function, args, columns, .. } => {
                let below = self.node(input)?;
                let name = plan.string(function);
                if TableFunction::lookup(name) == Some(TableFunction::Unnest) {
                    let unnest =
                        LateralUnnest::new(plan, &below.schema, index, args, columns, self.cancel)?
                            .in_session(self.session);
                    let schema = unnest.schema().clone();
                    let counters = self.watch(reference, id, pipeline, "Unnest", None);
                    return Ok(below.then(Arc::new(Watched::new(unnest, counters)), schema));
                }
                let lateral = LateralSeries::new(
                    plan,
                    &below.schema,
                    index,
                    name,
                    args,
                    columns,
                    self.cancel,
                )?
                .in_session(self.session);
                let schema = lateral.schema().clone();
                let counters = self.watch(reference, id, pipeline, "Series", Some(name));
                below.then(Arc::new(Watched::new(lateral, counters)), schema)
            }
            Node::Fetch { input, index, args, columns, row } => {
                let below = self.node(input)?;
                let counters = self.watch(reference, id, pipeline, "Fetch", None);
                let fetch = Fetch::new(plan, &below.schema, index, args, columns, row)?
                    .in_session(self.session)
                    .watched(counters.clone());
                let schema = fetch.schema().clone();
                below.then(Arc::new(Watched::new(fetch, counters)), schema)
            }
            Node::TableFetch { input, index, catalog, schema, table, columns, row } => {
                let below = self.node(input)?;
                let name = QualifiedName::new(
                    plan.string(catalog),
                    plan.string(schema),
                    plan.string(table),
                );
                let counters = self.watch(reference, id, pipeline, "TableFetch", None);
                let fetch = TableFetch::new(
                    plan,
                    &below.schema,
                    index,
                    self.catalog.table(&name)?,
                    columns,
                    row,
                )?
                .in_session(self.session);
                let schema = fetch.schema().clone();
                below.then(Arc::new(Watched::new(fetch, counters)), schema)
            }
            Node::Filter { input, predicate } => {
                let marks = self.marking == Some(reference);
                self.pruning = rudb_opt::bounds::of(plan, input, predicate);
                // Which filters can go is not decided here, because `EXPLAIN` has to say the same
                // thing about the same plan and a second copy of the condition is a second chance
                // to answer it differently.
                self.pushing = rudb_opt::bounds::into_scan(plan, reference).map(|moved| Pushdown {
                    node: reference,
                    predicate,
                    tests: moved.tests,
                    whole: moved.whole,
                    conjuncts: moved.conjuncts,
                    marks,
                });
                // Whether there was an offer at all, held here because afterwards the field says
                // only whether there is one now. Gone can mean taken or it can mean never made, and
                // reading the second as the first is this filter deleting itself.
                let offered = self.pushing.is_some();
                let below = match count_having_aggregate(plan, input, predicate) {
                    Some((aggregate, call, minimum)) => {
                        let Node::Aggregate { input: under, index, groups, aggregates } =
                            *plan.node(aggregate)
                        else {
                            unreachable!("count_having_aggregate returned another node")
                        };
                        self.aggregate(
                            aggregate,
                            under,
                            index,
                            groups,
                            aggregates,
                            AggregateBound {
                                max_groups: None,
                                top_counts: None,
                                having_count: Some((call, minimum)),
                            },
                        )?
                    }
                    None => self.node(input)?,
                };
                // Cleared whether or not the scan arm took them, because a filter over anything
                // else leaves them sitting there for whatever scan the walk reaches next.
                self.pruning = Vec::new();
                // And the offer taken back, for the same reason. An offer that was made and is no
                // longer there is one the scan below took, which means it is applying this predicate
                // itself and there is no operator to build here. Anything else and the filter runs
                // where it always did.
                let taken = offered && self.pushing.take().is_none();
                self.pushing = None;
                if taken {
                    return Ok(below);
                }
                let schema = below.schema.clone();
                let filter = Filter::new(plan, reference, predicate, &schema, self.seams)?
                    .in_session(self.session)
                    .marking(marks);
                let counters = self.watch(reference, id, pipeline, "Filter", None);
                below.then(Arc::new(Watched::new(filter, counters)), schema)
            }
            Node::Project { input, index, exprs, names } => {
                let below = self.node(input)?;
                let project = Project::new(plan, &below.schema, index, exprs, names)?
                    .in_session(self.session);
                let schema = project.schema().clone();
                let counters = self.watch(reference, id, pipeline, "Project", None);
                below.then(Arc::new(Watched::new(project, counters)), schema)
            }
            Node::Aggregate { input, index, groups, aggregates } => {
                let top_counts = self.top_counts.iter().find_map(|&(aggregate, bound, call)| {
                    (aggregate == reference).then_some((bound, call))
                });
                self.aggregate(
                    reference,
                    input,
                    index,
                    groups,
                    aggregates,
                    AggregateBound { max_groups: None, top_counts, having_count: None },
                )?
            }
            Node::Sort { input, keys } => {
                let below = self.node(input)?;
                let schema = below.schema.clone();
                let (sort, out) = Sort::new(plan, &schema, keys, memory)?;
                let sort = sort.in_session(self.session);
                let counters = self.watch(reference, id, pipeline, "Sort", None);
                let reading = Arc::clone(&counters);
                self.close(below, pipeline, Arc::new(Watched::new(sort, counters)));
                Segment::reading(Arc::new(Watched::new(out, reading)), schema, pipeline)
            }
            Node::Limit { input, count, offset } => {
                // Only a limit that is a pair of numbers here can cap the grouping below it. One
                // that reads its count off the rows does not have the number yet, and the point of
                // the cap is to stop the hash table growing before the first row comes out.
                let max_groups = count
                    .rows()
                    .zip(offset.rows())
                    .and_then(|(count, offset)| count.checked_add(offset))
                    .and_then(|count| usize::try_from(count).ok());
                let below = match (plan.node(input).clone(), max_groups) {
                    (
                        Node::Aggregate { input: under, index, groups, aggregates },
                        Some(max_groups),
                    ) => self.aggregate(
                        input,
                        under,
                        index,
                        groups,
                        aggregates,
                        AggregateBound {
                            max_groups: Some(max_groups),
                            top_counts: None,
                            having_count: None,
                        },
                    )?,
                    _ => self.node(input)?,
                };
                let schema = below.schema.clone();
                let limit = Limit::new(edge(plan, count, &schema)?, edge(plan, offset, &schema)?)
                    .in_session(self.session);
                let counters = self.watch(reference, id, pipeline, "Limit", None);
                below.then(Arc::new(Watched::new(limit, counters)), schema)
            }
            Node::LimitPercent { input, percent, offset } => {
                // A breaker rather than a stream, because a share of the input is not known until
                // the input has ended. So the pipeline below this one closes here and the rows come
                // back out of the buffer the finish fills.
                let below = self.node(input)?;
                let schema = below.schema.clone();
                let (limit, out) = LimitPercent::new(
                    portion(plan, percent, &schema)?,
                    edge(plan, offset, &schema)?,
                    memory,
                    self.session,
                );
                let counters = self.watch(reference, id, pipeline, "LimitPercent", None);
                let reading = Arc::clone(&counters);
                self.close(below, pipeline, Arc::new(Watched::new(limit, counters)));
                Segment::reading(Arc::new(Watched::new(out, reading)), schema, pipeline)
            }
            Node::TopN { input, keys, count, offset } => {
                if let Some((aggregate, call)) = count_top_aggregate(plan, input, keys) {
                    let bound = count.saturating_add(offset);
                    if let Ok(bound) = usize::try_from(bound) {
                        self.top_counts.push((aggregate, bound, call));
                    }
                }
                // Made before the input is walked into, because the scan at the bottom of it takes
                // a reader on this while it is built and the top N only fills it while the query
                // runs. It stays inert unless the arming below finds a scan column the ordering is
                // on, which is a scan that reads everything exactly as it did before.
                let cutoff = Cutoff::new();
                self.cutoff = Some(Arc::clone(&cutoff));
                let below = self.node(input)?;
                self.cutoff = None;
                // Armed afterwards, like the join's own filter and for the same reason: the binding
                // the top N knows is the one the projection above the scan hands it, so it has to be
                // walked down to the scan's own before the scan can be asked about it.
                if let Some((binding, op)) = cutoff::ordering(plan, keys)
                    && let Some(binding) = sideways::beneath(plan, input, binding)
                {
                    cutoff.about(binding, op);
                }
                let schema = below.schema.clone();
                let (top, out) = TopN::new(plan, &schema, keys, count, offset, memory)?;
                let top = top.telling(cutoff).in_session(self.session);
                let counters = self.watch(reference, id, pipeline, "TopN", None);
                let reading = Arc::clone(&counters);
                self.close(below, pipeline, Arc::new(Watched::new(top, counters)));
                Segment::reading(Arc::new(Watched::new(out, reading)), schema, pipeline)
            }
            Node::Distinct { input, on } => {
                let below = self.node(input)?;
                let schema = below.schema.clone();
                let (distinct, out) = Distinct::new(plan, &schema, on, memory)?;
                let distinct = distinct.in_session(self.session);
                let counters = self.watch(reference, id, pipeline, "Distinct", None);
                let reading = Arc::clone(&counters);
                self.close(below, pipeline, Arc::new(Watched::new(distinct, counters)));
                Segment::reading(Arc::new(Watched::new(out, reading)), schema, pipeline)
            }
            // One pipeline rather than two, which is the whole of what this node buys. The parent
            // is not walked into at all: it is read column by column out of the catalog, and what
            // the child's rows carry away from it is a row id per row and a pointer per column.
            Node::LinkJoin { child, parent, kind, conditions, rid } => {
                let found = self.linked(reference, child, parent, conditions)?;
                let below = self.node(child)?;
                // A parent key column is the child's key column over the rows an inner join keeps,
                // when both are the same type, so it is taken from the child rather than gathered.
                let child_types = below.schema.types();
                let parent_types = found.parent_schema.types();
                let keys: Vec<(usize, usize)> = found
                    .keys
                    .iter()
                    .filter_map(|&(child_key, parent_key)| {
                        let at = below.schema.position_of(child_key)?;
                        let taken = found.parent_schema.position_of(parent_key)?;
                        (child_types.get(at)? == parent_types.get(taken)?).then_some((taken, at))
                    })
                    .collect();
                let operator = LinkJoin::new(
                    plan,
                    kind,
                    found.link,
                    found.parent,
                    found.projected,
                    rid,
                    &below.schema,
                    &found.parent_schema,
                    self.seams,
                    memory,
                    self.cancel.clone(),
                )?
                .taking_keys(&keys)
                .in_session(self.session);
                let schema = operator.schema().clone();
                let counters = self.watch(reference, id, pipeline, "LinkJoin", None);
                below.then(Arc::new(Watched::new(operator, counters)), schema)
            }
            Node::Join { .. } => self.join(reference)?,
            Node::CrossProduct { left, right } => {
                // The right side runs first and is kept as the chunks it arrived in, because it is
                // replayed once per left row. The left side streams, which is the whole point of
                // this operator: the product is produced a chunk at a time and never held, so the
                // product stays in the pipeline the left rows came from rather than starting one.
                let keep_id = self.gathered(reference);
                let aside = self.shape.pipeline(right);
                let right = self.node(right)?;
                let right_schema = right.schema.clone();
                let (keep, kept) = Keep::new(memory);
                let held = self.watch(reference, keep_id, aside, "Keep", None);
                self.close(right, aside, Arc::new(Watched::new(keep, held)));
                let mut left = self.node(left)?;
                let cross = CrossProduct::new(&left.schema, &right_schema, kept);
                let schema = cross.schema().clone();
                let counters = self.watch(reference, id, pipeline, "CrossProduct", None);
                left.after.push(aside);
                left.then(Arc::new(Watched::new(cross, counters)), schema)
            }
            Node::SetOp { left, right, kind, all, index } => {
                // The right side runs first, because nothing can be said about a left row until the
                // whole right side has been counted. That is the dependency edge, spelled out.
                let gather_id = self.gathered(reference);
                let counting = self.shape.pipeline(right);
                let right = self.node(right)?;
                let (gather, gathered) = Gather::new(memory);
                let kept = self.watch(reference, gather_id, counting, "Gather", None);
                self.close(right, counting, Arc::new(Watched::new(gather, kept)));
                let mut left = self.node(left)?;
                let (setop, out) = SetOp::new(&left.schema, gathered, kind, all, index, memory);
                let schema = setop.schema().clone();
                let counters = self.watch(reference, id, pipeline, "SetOp", None);
                let reading = Arc::clone(&counters);
                left.after.push(counting);
                self.close(left, pipeline, Arc::new(Watched::new(setop, counters)));
                Segment::reading(Arc::new(Watched::new(out, reading)), schema, pipeline)
            }
            // Unnesting either removes this node or refuses the query, and it runs before every
            // other pass, so nothing a query can be written as arrives here. What is left is a
            // pass that built one, which is a bug in that pass rather than a gap anyone wrote.
            Node::DependentJoin { .. } => {
                return Err(Error::internal(
                    "a dependent join reached execution before subquery unnesting",
                ));
            }
            Node::Window { input, index, partition, order, frame, expressions } => {
                let below = self.node(input)?;
                let written = Written { index, partition, order, frame, expressions };
                let (window, out) = Window::new(plan, &below.schema, &written, memory)?;
                let window = window.in_session(self.session);
                let schema = window.schema().clone();
                let counters = self.watch(reference, id, pipeline, "Window", None);
                let reading = Arc::clone(&counters);
                self.close(below, pipeline, Arc::new(Watched::new(window, counters)));
                Segment::reading(Arc::new(Watched::new(out, reading)), schema, pipeline)
            }
            Node::MaterializedCte { definition, body, cte, .. } => {
                // The definition runs first and the rows are held, which is what the word
                // materialized asked for. This node is the sink of the pipeline that fills them,
                // the same way a sort is the sink of the pipeline under it, and the body carries on
                // in whatever pipeline the parent was in because it is never held.
                //
                // The pipeline is closed before the body is walked, so it is on the list ahead of
                // everything the body builds and the rows exist by the time anything reads them.
                let held = self.node(definition)?;
                let (keep, chunks) = Keep::new(memory);
                let counters = self.watch(reference, id, pipeline, "MaterializedCTE", None);
                self.close(held, pipeline, Arc::new(Watched::new(keep, counters)));
                self.held.push(Held { cte, chunks, filling: pipeline });
                let segment = self.node(body);
                self.held.pop();
                segment?
            }
            Node::Consistent { index, columns, reducer } => {
                // One pipeline per relation, children of the join tree first, each ending in the
                // sink that runs the first sweep over it, and then this node as the source of the
                // one row. Each relation's pipeline waits for its children's, because its sink
                // reads the keys they kept. See `crate::consistent`.
                let tree = plan.reducer(reducer);
                let fields = plan.field_list(columns).to_vec();
                let types = fields.iter().map(|field| field.ty.clone()).collect();
                let shared = Arc::new(Reduction::new(tree, types, memory)?);
                let mut filled: Vec<PipelineRef> = Vec::with_capacity(tree.leaves.len());
                for (at, leaf) in tree.leaves.iter().enumerate() {
                    let own = self.shape.pipeline(leaf.input);
                    let mut below = self.node(leaf.input)?;
                    let position = u32::try_from(at).map_err(|_| {
                        Error::internal("a join tree of more than u32::MAX relations")
                    })?;
                    below
                        .after
                        .extend(tree.children(position).map(|(child, _)| filled[child as usize]));
                    self.close(below, own, Arc::new(Collect::new(Arc::clone(&shared), at)));
                    filled.push(own);
                }
                let answer = Answer::new(shared, Schema::numbered(fields, index));
                let schema = answer.schema().clone();
                let counters = self.watch(reference, id, pipeline, "Consistent", None);
                Segment {
                    source: Arc::new(Watched::new(answer, counters)),
                    streams: Vec::new(),
                    schema,
                    after: filled,
                }
            }
            Node::CteScan { index, cte, columns, .. } => {
                // A read of the held rows, which is a leaf the same way a scan of a table is. Each
                // one takes a reader of its own, because the rows were held so that every read gets
                // all of them and a shared cursor would split one pass between the reads instead.
                //
                // The columns are bound against this node's own index rather than the definition's,
                // which is what everything above it was bound against.
                let Some(source) = self.held.iter().rev().find(|held| held.cte == cte) else {
                    return Err(Error::internal(
                        "a read of a materialisation that is not being filled",
                    ));
                };
                let filling = source.filling;
                let source = source.chunks.reader();
                let schema = Schema::numbered(plan.field_list(columns).to_vec(), index);
                let counters = self.watch(reference, id, pipeline, "CteScan", None);
                Segment::reading(Arc::new(Watched::new(source, counters)), schema, filling)
            }
        };
        Ok(segment)
    }
}

/// Whether the filter under an aggregate can mark the rows it keeps rather than cut them out.
///
/// The aggregate then reads its keys at the kept rows alone and its arguments over the whole chunk,
/// and counts a dropped row into no group. That is sound only where reading an argument at a row
/// the filter dropped changes nothing, so every key and argument has to be free of calls like
/// `nextval` whose answer is not decided by their arguments. An argument that raises at a dropped
/// row is read again at the kept rows alone, see `Aggregate::read`. A `DISTINCT` call and an
/// aggregate with no groups read their rows through paths of their own and are left as they were.
fn marks_through(plan: &Plan, input: NodeRef, groups: Slice, aggregates: Slice) -> bool {
    if !matches!(plan.node(input), Node::Filter { .. }) {
        return false;
    }
    let keys = plan.expr_list(groups);
    let calls = plan.expr_list(aggregates);
    let plain = calls
        .iter()
        .all(|&call| matches!(plan.expr(call), Expr::Aggregate { distinct: false, .. }));
    !keys.is_empty()
        && plain
        && !keys.iter().chain(calls).any(|&expr| rudb_opt::volatile(plan, expr))
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::path::PathBuf;
    use std::time::{SystemTime, UNIX_EPOCH};

    use rudb_catalog::Catalog;
    use rudb_common::{Field, LogicalType, Value};
    use rudb_plan::{CompareOp, Expr, Node, Plan};
    use rudb_vector::{Chunk, VECTOR_SIZE, Vector};

    use super::{count_having_aggregate, count_top_aggregate, native_pair_frequencies};

    fn native_path(label: &str) -> PathBuf {
        let stamp = SystemTime::now().duration_since(UNIX_EPOCH).expect("time advances").as_nanos();
        std::env::temp_dir().join(format!("rudb-exec-{label}-{}-{stamp}.rdb", std::process::id()))
    }

    fn native_catalog(label: &str, rows: &[(i64, String)]) -> (PathBuf, Catalog) {
        let path = native_path(label);
        let fields = vec![
            Field::required("id", LogicalType::BigInt),
            Field::required("phrase", LogicalType::Varchar),
        ];
        let mut writer = rudb_native::Writer::create(&path, "items", fields).expect("new file");
        for rows in rows.chunks(VECTOR_SIZE) {
            let ids = rows.iter().map(|(id, _)| Value::BigInt(*id)).collect::<Vec<_>>();
            let phrases =
                rows.iter().map(|(_, phrase)| Value::Varchar(phrase.clone())).collect::<Vec<_>>();
            let chunk = Chunk::new(vec![
                Vector::from_values(LogicalType::BigInt, &ids).expect("big integers"),
                Vector::from_values(LogicalType::Varchar, &phrases).expect("strings"),
            ])
            .expect("matching columns");
            writer.append(&chunk).expect("rows");
        }
        writer.finish().expect("commit");
        let native = rudb_native::Catalog::open(&path).expect("reopen");
        let mut catalog = Catalog::new();
        catalog.create_native_table(native.table("items").expect("stored table")).expect("attach");
        (path, catalog)
    }

    fn pair_plan() -> Plan {
        Plan::parse(
            "Aggregate #1 groups=[#0.0::BIGINT, #0.1::VARCHAR] \
             aggregates=[count_star()::BIGINT]\n  \
             Get memory.main.items AS items #0 [id::BIGINT, phrase::VARCHAR]",
        )
        .expect("a two-key grouped count")
    }

    fn pair_frequencies(
        plan: &Plan,
        catalog: &Catalog,
        top: usize,
    ) -> Option<super::NativePairFrequencies> {
        let Node::Aggregate { input, groups, aggregates, .. } = *plan.node(plan.root()) else {
            panic!("the root is an aggregate")
        };
        native_pair_frequencies(plan, catalog, input, groups, aggregates, top)
            .expect("metadata reads")
    }

    #[test]
    fn sparse_occurrences_compute_two_key_top_counts_at_query_time() {
        let mut rows = Vec::new();
        rows.extend(std::iter::repeat_n((1, "a".to_string()), VECTOR_SIZE + 5));
        rows.extend(std::iter::repeat_n((1, "b".to_string()), 4));
        rows.extend(std::iter::repeat_n((2, "x".to_string()), 3));
        rows.push((3, "y".to_string()));
        let (path, catalog) = native_catalog("pair-frequencies", &rows);
        let answer = pair_frequencies(&pair_plan(), &catalog, 2).expect("query-time result");
        assert_eq!(answer.entries.len(), 2);
        assert!(answer.entries.contains(&(
            vec![Value::BigInt(1), Value::Varchar("a".to_string())],
            u64::try_from(VECTOR_SIZE + 5).expect("a small vector width"),
        )));
        assert!(
            answer.entries.contains(&(vec![Value::BigInt(1), Value::Varchar("b".to_string())], 4))
        );
        fs::remove_file(path).expect("clean up");
    }

    #[test]
    fn sparse_occurrences_refuse_a_pair_tied_with_the_omitted_tail() {
        let rows = (0..513_i64).map(|id| (id, format!("phrase {id}"))).collect::<Vec<_>>();
        let (path, catalog) = native_catalog("pair-fallback", &rows);
        assert!(
            pair_frequencies(&pair_plan(), &catalog, 10).is_none(),
            "a count of one cannot beat an omitted first-key count of one"
        );
        fs::remove_file(path).expect("clean up");
    }

    fn plan(direction: &str) -> Plan {
        Plan::parse(&format!(
            "TopN 10 offset 0 [#2.2::BIGINT {direction} NULLS LAST]\n  \
             Project #2 [#1.0::BIGINT AS WatchID, #1.1::INTEGER AS ClientIP, #1.2::BIGINT AS c]\n    \
             Aggregate #1 groups=[#0.0::BIGINT, #0.1::INTEGER] \
             aggregates=[count_star()::BIGINT]\n      \
             Values #0 [WatchID::BIGINT, ClientIP::INTEGER] rows=[]"
        ))
        .expect("a grouped count plan")
    }

    #[test]
    fn count_descending_topn_marks_its_aggregate() {
        let plan = plan("DESC");
        let Node::TopN { input, keys, .. } = *plan.node(plan.root()) else {
            panic!("the root is a TopN")
        };
        let (aggregate, call) = count_top_aggregate(&plan, input, keys).expect("the grouped count");
        assert!(matches!(plan.node(aggregate), Node::Aggregate { .. }));
        assert_eq!(call, 0, "the count is the only call");
    }

    #[test]
    fn count_descending_topn_crosses_several_passthrough_projects() {
        let plan = Plan::parse(
            "TopN 10 offset 0 [#3.1::BIGINT DESC NULLS LAST]\n  \
             Project #3 [#2.0::INTEGER AS ClientIP, #2.1::BIGINT AS c]\n    \
             Project #2 [#1.0::INTEGER AS column0, #1.1::BIGINT AS column1]\n      \
             Aggregate #1 groups=[#0.0::INTEGER] aggregates=[count_star()::BIGINT]\n        \
             Values #0 [ClientIP::INTEGER] rows=[]",
        )
        .expect("a grouped count under two projects");
        let Node::TopN { input, keys, .. } = *plan.node(plan.root()) else {
            panic!("the root is a TopN")
        };
        let (aggregate, call) = count_top_aggregate(&plan, input, keys).expect("the grouped count");
        assert!(matches!(plan.node(aggregate), Node::Aggregate { .. }));
        assert_eq!(call, 0, "the count is the only call");
    }

    #[test]
    fn count_ascending_cannot_discard_large_counts() {
        let plan = plan("ASC");
        let Node::TopN { input, keys, .. } = *plan.node(plan.root()) else {
            panic!("the root is a TopN")
        };
        assert!(count_top_aggregate(&plan, input, keys).is_none());
    }

    #[test]
    fn distinct_count_descending_topn_marks_its_aggregate() {
        let plan = Plan::parse(
            "TopN 10 offset 0 [#1.1::BIGINT DESC NULLS LAST]\n  \
             Aggregate #1 groups=[#0.0::VARCHAR] \
             aggregates=[count(DISTINCT #0.1::BIGINT)::BIGINT]\n    \
             Values #0 [SearchPhrase::VARCHAR, UserID::BIGINT] rows=[]",
        )
        .expect("a grouped distinct count plan");
        let Node::TopN { input, keys, .. } = *plan.node(plan.root()) else {
            panic!("the root is a TopN")
        };
        let (aggregate, call) =
            count_top_aggregate(&plan, input, keys).expect("the distinct count");
        assert!(matches!(plan.node(aggregate), Node::Aggregate { .. }));
        assert_eq!(call, 0, "the distinct count is the only call");
    }

    #[test]
    fn count_descending_topn_finds_a_later_aggregate_call() {
        let plan = Plan::parse(
            "TopN 10 offset 0 [#1.2::BIGINT DESC NULLS LAST]\n  \
             Aggregate #1 groups=[#0.0::INTEGER] \
             aggregates=[sum(#0.1::SMALLINT)::HUGEINT, count_star()::BIGINT, avg(#0.2::SMALLINT)::DOUBLE, count(DISTINCT #0.3::BIGINT)::BIGINT]\n    \
             Values #0 [RegionID::INTEGER, AdvEngineID::SMALLINT, ResolutionWidth::SMALLINT, UserID::BIGINT] rows=[]",
        )
        .expect("a mixed aggregate plan");
        let Node::TopN { input, keys, .. } = *plan.node(plan.root()) else {
            panic!("the root is a TopN")
        };
        let (aggregate, call) = count_top_aggregate(&plan, input, keys).expect("the grouped count");
        assert!(matches!(plan.node(aggregate), Node::Aggregate { .. }));
        assert_eq!(call, 1, "the count is the second of the four calls");
    }

    #[test]
    fn a_count_having_lower_bound_marks_the_count_call() {
        let plan = Plan::parse(
            "Filter (#1.2::BIGINT > 100::BIGINT)::BOOLEAN\n  \
             Aggregate #1 groups=[#0.0::BIGINT] \
             aggregates=[avg(#0.1::BIGINT)::DOUBLE, count_star()::BIGINT]\n    \
             Values #0 [key::BIGINT, value::BIGINT] rows=[]",
        )
        .expect("an aggregate with a HAVING filter");
        let Node::Filter { input, predicate } = *plan.node(plan.root()) else {
            panic!("the root is a Filter")
        };
        let (aggregate, call, minimum) =
            count_having_aggregate(&plan, input, predicate).expect("the count bound");
        assert_eq!(aggregate, input);
        assert_eq!((call, minimum), (1, 101));
    }

    #[test]
    fn an_upper_count_having_bound_cannot_drop_aggregate_output() {
        let mut plan = Plan::parse(
            "Filter (#1.1::BIGINT > 100::BIGINT)::BOOLEAN\n  \
             Aggregate #1 groups=[#0.0::BIGINT] aggregates=[count_star()::BIGINT]\n    \
             Values #0 [key::BIGINT] rows=[]",
        )
        .expect("an aggregate with a HAVING filter");
        let Node::Filter { input, predicate } = *plan.node(plan.root()) else {
            panic!("the root is a Filter")
        };
        let Expr::Compare { left, right, .. } = *plan.expr(predicate) else {
            panic!("the predicate is a comparison")
        };
        let less =
            plan.add_expr(Expr::Compare { op: CompareOp::Less, left, right }, LogicalType::Boolean);
        assert!(count_having_aggregate(&plan, input, less).is_none());
    }
}
