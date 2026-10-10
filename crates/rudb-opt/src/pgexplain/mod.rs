//! `EXPLAIN` in the format PostgreSQL writes, a port of `explain.c`.
//!
//! The plan is rudb's plan and not the plan PostgreSQL would choose, so the content of the output
//! is not PostgreSQL's (document 08 section 8.13). The shape is: the node names, the keys and their
//! order, the indents and the numbers' formats are those of `explain.c`, because pgAdmin, sqlx and
//! every other tool that reads a plan parses them.
//!
//! The output is made in two steps. `Builder` maps each rudb operator to the PostgreSQL node it
//! is closest to, which is sometimes two nodes (a hash join is a `Hash Join` over a `Hash`) and
//! sometimes none (a projection is the output of the node under it). An operator that has no
//! PostgreSQL name keeps the closest name and carries its own name in a `rudb` key. Then
//! `Builder::write` writes the nodes in the order `ExplainNode` writes them, through the writer
//! of [`format`].
//!
//! The estimates are the ones PostgreSQL makes for a table it has not analyzed, which is every
//! temporary table and every table a test has just made: ten pages, as many rows as the width of a
//! row fits on them, and the default selectivities of `selfuncs.h`. The costs follow from those
//! with the default cost constants of `costsize.c`.

mod deparse;
mod format;

use std::cell::RefCell;
use std::collections::{HashMap, HashSet};

use rudb_common::stat::Stat;
use rudb_common::{LogicalType, Value};
use rudb_metrics::{Document, Operator};
use rudb_pgtypes::keywords::quote_identifier;
use rudb_pgtypes::{OutputSettings, oid, pg_type};
use rudb_plan::explain::{Options, Serialize};
use rudb_plan::{
    Bound, BuildSide, ColumnBinding, CompareOp, ConjunctionOp, Expr, ExprRef, JoinKind, Node,
    NodeRef, Plan, SetOpKind, Shape, SortKey, WindowBound, WindowExclude, WindowFrame, WindowUnit,
};

use self::deparse::{Deparse, Names, function_name};
use self::format::{Writer, fixed};
use crate::estimate::{Facts, rows_stat};

/// `seq_page_cost`.
const SEQ_PAGE_COST: f64 = 1.0;
/// `cpu_tuple_cost`.
const CPU_TUPLE_COST: f64 = 0.01;
/// `cpu_operator_cost`.
const CPU_OPERATOR_COST: f64 = 0.0025;
/// `DEFAULT_NUM_DISTINCT`, the distinct values of a column nobody counted.
const DEFAULT_NUM_DISTINCT: f64 = 200.0;
/// The rows of a set returning function with no support function, its `prorows`.
const DEFAULT_FUNCTION_ROWS: f64 = 1000.0;
/// `BLCKSZ` less `SizeOfPageHeaderData`.
const USABLE_BYTES_PER_PAGE: f64 = 8168.0;
/// `MAXALIGN(SizeofHeapTupleHeader)` and `sizeof(ItemIdData)`.
const OVERHEAD_BYTES_PER_TUPLE: f64 = 28.0;
/// The pages PostgreSQL assumes for a table that has never been vacuumed or analyzed.
const UNANALYZED_PAGES: f64 = 10.0;
/// The rounds of a recursive union PostgreSQL assumes.
const RECURSIVE_ROUNDS: f64 = 10.0;

/// The buffer counts of `show_buffer_usage`, in its order.
const BUFFER_KEYS: [&str; 10] = [
    "Shared Hit Blocks",
    "Shared Read Blocks",
    "Shared Dirtied Blocks",
    "Shared Written Blocks",
    "Local Hit Blocks",
    "Local Read Blocks",
    "Local Dirtied Blocks",
    "Local Written Blocks",
    "Temp Read Blocks",
    "Temp Written Blocks",
];

/// The counts of `show_wal_usage`, in its order.
const WAL_KEYS: [&str; 5] =
    ["WAL Records", "WAL FPI", "WAL Bytes", "WAL FPI Bytes", "WAL Buffers Full"];

/// The types of all the columns of a table, by catalog, schema and name.
pub type Columns<'a> = dyn Fn(&str, &str, &str) -> Option<Vec<LogicalType>> + 'a;

/// What a run of the statement measured, for `EXPLAIN ANALYZE`.
#[derive(Debug)]
pub struct Run<'a> {
    /// The operators' rows and times.
    pub document: &'a Document,
    /// The time of the whole run.
    pub execution_ns: u64,
    /// The size of the rows the run produced, as the client would get them.
    pub output_bytes: u64,
}

/// One `EXPLAIN` to write.
pub struct Explain<'a> {
    pub plan: &'a Plan,
    pub facts: &'a Facts,
    pub options: &'a Options,
    /// How a constant in an expression is written.
    pub settings: &'a OutputSettings<'a>,
    /// The types of all the columns of a table, by catalog, schema and name. The width of a whole
    /// row is what PostgreSQL divides a page by to guess the rows of a table, and a scan reads
    /// only the columns the query uses.
    pub table: &'a Columns<'a>,
    /// The time from the start of the parse to the end of the optimizer.
    pub planning_ns: u64,
    pub run: Option<Run<'a>>,
}

impl std::fmt::Debug for Explain<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Explain").field("options", self.options).finish_non_exhaustive()
    }
}

impl Explain<'_> {
    /// The whole output, which is `ExplainOnePlan`. Text and YAML are one row per line, so the
    /// caller splits them. There is no line end after the last line.
    #[must_use]
    pub fn render(&self) -> String {
        let builder = Builder {
            plan: self.plan,
            facts: self.facts,
            options: self.options,
            names: Names::of(self.plan),
            settings: self.settings,
            table: self.table,
            shape: Shape::of(self.plan),
            document: self.run.as_ref().map(|run| run.document),
            recursive: RefCell::new(Vec::new()),
            ctes: RefCell::new(HashMap::new()),
        };
        let root = builder.node(self.plan.root(), None);
        let options = self.options;
        let mut w = Writer::new(options.format);
        w.begin_output();
        w.open_group("Query", None, true);
        builder.write(&mut w, &root, None, None);
        // `ExplainPrintSettings`. rudb has no setting a plan depends on that is not at its default,
        // so the list is always empty, which the text format does not write at all.
        if options.settings && !w.text() {
            w.open_group("Settings", Some("Settings"), true);
            w.close_group("Settings", true);
        }
        let buffers = options.buffers && !w.text();
        if buffers || options.memory {
            w.open_group("Planning", Some("Planning"), true);
            if w.text() {
                w.indent_text();
                w.out.push_str("Planning:\n");
                w.indent += 1;
            }
            if buffers {
                zeros(&mut w, &BUFFER_KEYS);
            }
            if options.memory {
                let used = (self.plan.node_count() * size_of::<Node>()
                    + self.plan.expr_count() * size_of::<Expr>()) as u64;
                let allocated = used.div_ceil(8192).max(1) * 8192;
                memory(&mut w, kilobytes(used), kilobytes(allocated));
            }
            if w.text() {
                w.indent -= 1;
            }
            w.close_group("Planning", true);
        }
        if options.summary {
            w.property_float("Planning Time", Some("ms"), milliseconds(self.planning_ns), 3);
        }
        if options.analyze {
            w.open_group("Triggers", Some("Triggers"), false);
            w.close_group("Triggers", false);
        }
        if let (Some(run), true) = (&self.run, options.serialize != Serialize::None) {
            serialization(&mut w, options, run.output_bytes);
        }
        if let (Some(run), true) = (&self.run, options.summary && options.analyze) {
            w.property_float("Execution Time", Some("ms"), milliseconds(run.execution_ns), 3);
        }
        w.close_group("Query", true);
        w.end_output();
        let end = w.out.trim_end_matches('\n').len();
        w.out.truncate(end);
        w.out
    }
}

/// `ExplainPrintSerialize`. The time is not measured apart from the run, so it is not written.
fn serialization(w: &mut Writer, options: &Options, bytes: u64) {
    let format = if options.serialize == Serialize::Binary { "binary" } else { "text" };
    w.open_group("Serialization", Some("Serialization"), true);
    if w.text() {
        w.indent_text();
        let time = if options.timing { "time=0.000 ms  " } else { "" };
        w.out.push_str(&format!(
            "Serialization: {time}output={}kB  format={format}\n",
            kilobytes(bytes)
        ));
    } else {
        if options.timing {
            w.property_float("Time", Some("ms"), 0.0, 3);
        }
        w.property_uinteger("Output Volume", Some("kB"), kilobytes(bytes));
        w.property_text("Format", format);
        if options.buffers {
            zeros(w, &BUFFER_KEYS);
        }
    }
    w.close_group("Serialization", true);
}

/// `show_memory_counters`.
fn memory(w: &mut Writer, used: u64, allocated: u64) {
    if w.text() {
        w.indent_text();
        w.out.push_str(&format!("Memory: used={used}kB  allocated={allocated}kB\n"));
    } else {
        w.property_uinteger("Memory Used", Some("kB"), used);
        w.property_uinteger("Memory Allocated", Some("kB"), allocated);
    }
}

/// Counts rudb does not keep, written as zeros. The text format writes only counts that are not
/// zero, so it writes none of them.
fn zeros(w: &mut Writer, keys: &[&str]) {
    if !w.text() {
        for key in keys {
            w.property_integer(key, None, 0);
        }
    }
}

/// `BYTES_TO_KILOBYTES`, which rounds up.
fn kilobytes(bytes: u64) -> u64 {
    bytes.div_ceil(1024)
}

#[expect(clippy::cast_precision_loss, reason = "a time is far below 2^52 nanoseconds")]
fn milliseconds(ns: u64) -> f64 {
    ns as f64 / 1_000_000.0
}

/// `clamp_row_est`: at least one row and a whole number.
fn clamp(rows: f64) -> f64 {
    if rows <= 1.0 || rows.is_nan() { 1.0 } else { rows.round() }
}

/// `get_typavgwidth`: the size of a fixed length type, and a guess for the others.
fn width(ty: &LogicalType) -> f64 {
    let typed = pg_type(ty);
    if let Some(info) = typed.info().filter(|info| info.len > 0) {
        return f64::from(info.len);
    }
    let typmod = typed.typmod;
    let maximum = match typed.oid {
        oid::BPCHAR | oid::VARCHAR if typmod > 4 => (typmod - 4) * 4 + 4,
        oid::NUMERIC if typmod >= 4 => 8 + ((((typmod - 4) >> 16) & 0xffff) + 6) / 4 * 2,
        _ => 0,
    };
    let guess = match maximum {
        0 => 32,
        _ if typed.oid == oid::BPCHAR || maximum <= 32 => maximum,
        _ if maximum < 1000 => 32 + (maximum - 32) / 2,
        _ => 32 + (1000 - 32) / 2,
    };
    f64::from(guess)
}

/// `LOG2`, of a count that is at least two.
fn log2(count: f64) -> f64 {
    count.max(2.0).log2()
}

/// The inputs of a node, which for [`Node::Consistent`] are the relations it reads.
pub(super) fn inputs(plan: &Plan, at: NodeRef) -> Vec<NodeRef> {
    match *plan.node(at) {
        Node::Consistent { reducer, .. } => {
            plan.reducer(reducer).leaves.iter().map(|leaf| leaf.input).collect()
        }
        ref other => other.children().into_iter().flatten().collect(),
    }
}

/// The columns a node produces with their types. A semi, anti or mark join produces the columns of
/// the side it keeps.
pub(super) fn outputs(plan: &Plan, at: NodeRef) -> Vec<(ColumnBinding, LogicalType)> {
    let fields = |index: u32, columns| {
        plan.field_list(columns)
            .iter()
            .enumerate()
            .map(move |(position, field)| (binding(index, position), field.ty.clone()))
            .collect::<Vec<_>>()
    };
    let listed = |index: u32, from: usize, exprs| {
        plan.expr_list(exprs)
            .iter()
            .enumerate()
            .map(|(position, &expr)| {
                (binding(index, from + position), plan.expr_type(expr).clone())
            })
            .collect::<Vec<_>>()
    };
    match *plan.node(at) {
        Node::Get { index, columns, .. }
        | Node::Values { index, columns, .. }
        | Node::TableFunction { index, columns, .. }
        | Node::Fetch { index, columns, .. }
        | Node::TableFetch { index, columns, .. }
        | Node::CteScan { index, columns, .. }
        | Node::RecursiveCte { index, columns, .. }
        | Node::Consistent { index, columns, .. } => fields(index, columns),
        Node::Dummy => Vec::new(),
        Node::Project { index, exprs, .. } => listed(index, 0, exprs),
        Node::Aggregate { index, groups, aggregates, .. } => {
            let mut found = listed(index, 0, groups);
            let width = found.len();
            found.extend(listed(index, width, aggregates));
            found
        }
        Node::Window { input, index, expressions, .. } => {
            let mut found = outputs(plan, input);
            found.extend(listed(index, 0, expressions));
            found
        }
        Node::LateralFunction { input, index, columns, .. } => {
            let mut found = outputs(plan, input);
            found.extend(fields(index, columns));
            found
        }
        Node::Filter { input, .. }
        | Node::Sort { input, .. }
        | Node::Limit { input, .. }
        | Node::LimitPercent { input, .. }
        | Node::LimitTies { input, .. }
        | Node::TopN { input, .. }
        | Node::Distinct { input, .. }
        | Node::MaterializedCte { body: input, .. } => outputs(plan, input),
        Node::SetOp { left, index, .. } => outputs(plan, left)
            .into_iter()
            .enumerate()
            .map(|(position, (_, ty))| (binding(index, position), ty))
            .collect(),
        Node::Join { left, kind: JoinKind::Semi | JoinKind::Anti | JoinKind::Mark, .. }
        | Node::DependentJoin {
            left,
            kind: JoinKind::Semi | JoinKind::Anti | JoinKind::Mark,
            ..
        }
        | Node::LinkJoin {
            child: left,
            kind: JoinKind::Semi | JoinKind::Anti | JoinKind::Mark,
            ..
        } => outputs(plan, left),
        Node::Join { left, right, .. }
        | Node::DependentJoin { left, right, .. }
        | Node::CrossProduct { left, right }
        | Node::LinkJoin { child: left, parent: right, .. } => {
            let mut found = outputs(plan, left);
            found.extend(outputs(plan, right));
            found
        }
    }
}

fn binding(index: u32, position: usize) -> ColumnBinding {
    ColumnBinding::new(index, u32::try_from(position).unwrap_or(u32::MAX))
}

/// The estimate of a node, in the units of `costsize.c`.
#[derive(Debug, Clone, Copy, Default)]
struct Cost {
    startup: f64,
    total: f64,
    rows: f64,
    width: f64,
}

/// What a node did in a run.
#[derive(Debug, Clone, Copy)]
struct Actual {
    startup_ns: u64,
    total_ns: u64,
    rows: u64,
}

/// A property of a node after its output, in the order `ExplainNode` writes them.
enum Prop {
    Text(&'static str, String),
    List(&'static str, Vec<String>),
    /// `show_instrumentation_count`.
    Removed(&'static str, u64),
    /// `show_sort_info`.
    Sort {
        method: &'static str,
        kb: u64,
    },
    /// `show_hash_info`.
    Hash {
        buckets: u64,
        kb: u64,
    },
    /// `show_hashagg_info`, with the memory when the node ran.
    HashAgg {
        kb: Option<u64>,
    },
    /// `show_storage_info`.
    Storage {
        kb: u64,
    },
}

/// Where a filter over a node can go.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Quals {
    /// A scan, whose filter names a column's relation only under `VERBOSE`.
    Scan,
    /// A join, an aggregate, a window or a `Result`, whose filter names a column's relation when
    /// the plan reads more than one or under `VERBOSE`.
    Upper,
    /// A node that has no filter of its own, which gets a `Result` over it for one.
    None,
}

/// The relation a scan reads, as `ExplainScanTarget` writes it.
struct Target {
    tag: Option<&'static str>,
    object: Option<String>,
    schema: Option<String>,
    refname: String,
}

/// One node of the output.
struct Shown {
    /// The name in the non-text formats.
    sname: &'static str,
    /// The name in the text format, before the join type.
    pname: &'static str,
    strategy: Option<&'static str>,
    partial: bool,
    target: Option<Target>,
    join: Option<&'static str>,
    command: Option<&'static str>,
    /// The rudb operator this node stands for, when PostgreSQL has no node for it.
    rudb: Option<&'static str>,
    cost: Cost,
    /// What the node did, under `ANALYZE`. Nothing there means it never ran.
    actual: Option<Actual>,
    /// The rows the operator was given, for the rows a filter it applies removed.
    rows_in: Option<u64>,
    output: Vec<String>,
    unique: Option<bool>,
    /// The properties before the filter: the keys, the conditions and the call.
    keys: Vec<Prop>,
    filter: Option<String>,
    removed: Option<u64>,
    /// The properties after the filter: what the node used in the run.
    info: Vec<Prop>,
    /// Whether the rows removed by the filter go after [`Shown::info`], as an aggregate has them.
    removed_last: bool,
    quals: Quals,
    /// Whether the node can compute expressions for its output, which a `Sort` or a `Hash`
    /// cannot.
    projects: bool,
    /// The table index whose columns are written as the expressions that compute them.
    own: Option<u32>,
    children: Vec<Child>,
}

struct Child {
    relationship: &'static str,
    plan_name: Option<String>,
    node: Shown,
}

impl Child {
    fn new(relationship: &'static str, node: Shown) -> Self {
        Self { relationship, plan_name: None, node }
    }
}

impl Shown {
    fn new(sname: &'static str, pname: &'static str, quals: Quals) -> Self {
        Self {
            sname,
            pname,
            strategy: None,
            partial: false,
            target: None,
            join: None,
            command: None,
            rudb: None,
            cost: Cost::default(),
            actual: None,
            rows_in: None,
            output: Vec::new(),
            unique: None,
            keys: Vec::new(),
            filter: None,
            removed: None,
            info: Vec::new(),
            removed_last: false,
            projects: quals != Quals::None,
            quals,
            own: None,
            children: Vec::new(),
        }
    }

    /// The node a scan of one relation is.
    fn scan(name: &'static str, target: Target, own: u32) -> Self {
        let mut shown = Self::new(name, name, Quals::Scan);
        shown.target = Some(target);
        shown.own = Some(own);
        shown
    }
}

/// The mapping of a plan to the nodes of the output.
struct Builder<'a> {
    plan: &'a Plan,
    facts: &'a Facts,
    options: &'a Options,
    names: Names,
    settings: &'a OutputSettings<'a>,
    table: &'a Columns<'a>,
    shape: Shape,
    document: Option<&'a Document>,
    /// The recursive CTEs whose recursive part is being built, with the rows of their anchor. A
    /// scan of one of them is a `WorkTable Scan`.
    recursive: RefCell<Vec<(u32, f64)>>,
    /// The rows of each CTE built so far, for the scans of it.
    ctes: RefCell<HashMap<u32, f64>>,
}

impl Builder<'_> {
    fn deparse(&self, prefix: bool, own: Option<u32>) -> Deparse<'_> {
        Deparse { plan: self.plan, names: &self.names, settings: self.settings, prefix, own }
    }

    /// A condition as a list of quals, cheapest first the way `order_qual_clauses` puts them.
    /// The sort is stable, so quals of the same cost stay in the order they were written.
    fn qual(&self, prefix: bool, own: Option<u32>, predicate: ExprRef) -> String {
        let deparse = self.deparse(prefix, own);
        let Expr::Conjunction { op: ConjunctionOp::And, children } = self.plan.expr(predicate)
        else {
            return deparse.expr(predicate);
        };
        let mut quals: Vec<(f64, ExprRef)> = self
            .plan
            .expr_list(*children)
            .iter()
            .map(|&child| (self.operators(child), child))
            .collect();
        quals.sort_by(|a, b| a.0.total_cmp(&b.0));
        anded(quals.into_iter().map(|(_, child)| deparse.expr(child)).collect())
    }

    /// Whether an output names a column's relation: when the plan reads more than one.
    fn output_prefix(&self) -> bool {
        self.names.relations() > 1
    }

    /// Whether a key or an upper filter names a column's relation.
    fn upper_prefix(&self) -> bool {
        self.names.relations() > 1 || self.options.verbose
    }

    fn analyze(&self) -> bool {
        self.document.is_some()
    }

    /// The measured operator of a node.
    fn operator(&self, at: NodeRef) -> Option<&Operator> {
        let id = self.shape.operator_of(at)?;
        self.find(id)
    }

    fn find(&self, id: u32) -> Option<&Operator> {
        self.document?.operators.iter().find(|operator| operator.id == id)
    }

    /// Sets what a node did from its rows and its own time, with the time of its children added,
    /// since a PostgreSQL node's time includes the time of the nodes under it.
    fn time(&self, shown: &mut Shown, rows: Option<u64>, own_ns: u64, blocking: bool) {
        if !self.analyze() {
            return;
        }
        let Some(rows) = rows else {
            shown.actual = None;
            return;
        };
        let below: u64 =
            shown.children.iter().filter_map(|child| child.node.actual).map(|a| a.total_ns).sum();
        let total_ns = own_ns + below;
        let startup_ns = match blocking {
            true => total_ns,
            false => shown
                .children
                .iter()
                .find(|child| child.relationship != "InitPlan")
                .and_then(|child| child.node.actual)
                .map_or(0, |a| a.startup_ns),
        };
        shown.actual = Some(Actual { startup_ns, total_ns, rows });
    }

    /// Sets what a node did from the operator of a plan node.
    fn measure(&self, shown: &mut Shown, at: NodeRef, blocking: bool) {
        let operator = self.operator(at);
        shown.rows_in = operator.map(|operator| operator.rows_in);
        self.time(
            shown,
            operator.map(|operator| operator.rows_out),
            operator.map_or(0, |operator| operator.wall_ns),
            blocking,
        );
    }

    fn memory_kb(&self, at: NodeRef) -> u64 {
        self.operator(at).map_or(0, |operator| kilobytes(operator.memory.high_water))
    }

    /// The output of a node and its width.
    fn natural(&self, shown: &mut Shown, at: NodeRef) {
        let columns = outputs(self.plan, at);
        shown.cost.width = columns.iter().map(|(_, ty)| width(ty)).sum();
        if self.options.verbose {
            let deparse = self.deparse(self.output_prefix(), self.plan.node(at).table_index());
            shown.output = columns.iter().map(|&(column, _)| deparse.column(column)).collect();
        }
    }

    /// The node for a plan node, with the projection over it if there is one.
    fn node(&self, at: NodeRef, project: Option<NodeRef>) -> Shown {
        let shown = self.raw(at);
        match project {
            Some(project) => self.projected(shown, project),
            None => shown,
        }
    }

    /// A projection, which is the output of the node under it. A node that cannot compute an
    /// expression gets a `Result` over it, which is what `create_projection_plan` does.
    fn projected(&self, mut shown: Shown, project: NodeRef) -> Shown {
        let Node::Project { input, index, exprs, .. } = *self.plan.node(project) else {
            return shown;
        };
        let list = self.plan.expr_list(exprs);
        let plain = list.iter().all(|&expr| matches!(self.plan.expr(expr), Expr::Column(_)));
        if !shown.projects && !plain {
            shown = self.result_over(shown);
        }
        let evaluated: f64 = list.iter().map(|&expr| self.operators(expr)).sum();
        shown.cost.total += evaluated * CPU_OPERATOR_COST * shown.cost.rows;
        let junk = self.junk(input, list, shown.own);
        shown.cost.width =
            list.iter().chain(&junk).map(|&expr| width(self.plan.expr_type(expr))).sum();
        if self.options.verbose {
            let deparse = self.deparse(self.output_prefix(), shown.own);
            shown.output = deparse.exprs(list);
            shown.output.extend(deparse.exprs(&junk));
        }
        if let (Some(operator), Some(actual)) = (self.operator(project), shown.actual.as_mut()) {
            actual.total_ns += operator.wall_ns;
        }
        shown.own = Some(index);
        shown
    }

    /// The keys a node sorts or lays out its rows on that a projection over it does not compute.
    /// PostgreSQL keeps them in the target list as `resjunk` entries, so they are in the output and
    /// the width of the node.
    fn junk(&self, input: NodeRef, list: &[ExprRef], own: Option<u32>) -> Vec<ExprRef> {
        let plan = self.plan;
        let mut keys: Vec<ExprRef> = match *plan.node(input) {
            Node::Sort { keys, .. } | Node::TopN { keys, .. } => {
                plan.sort_key_list(keys).iter().map(|key| key.expr).collect()
            }
            Node::Window { partition, order, .. } => {
                let mut keys = plan.expr_list(partition).to_vec();
                keys.extend(plan.sort_key_list(order).iter().map(|key| key.expr));
                keys
            }
            _ => return Vec::new(),
        };
        let deparse = self.deparse(false, own);
        let mut seen: HashSet<String> = deparse.exprs(list).into_iter().collect();
        keys.retain(|&key| seen.insert(deparse.expr(key)));
        keys
    }

    /// A `Result` over a node, for a filter or a projection the node cannot do itself.
    fn result_over(&self, child: Shown) -> Shown {
        let mut result = Shown::new("Result", "Result", Quals::Upper);
        result.cost = Cost {
            startup: child.cost.startup,
            total: child.cost.total + child.cost.rows * CPU_TUPLE_COST,
            rows: child.cost.rows,
            width: child.cost.width,
        };
        result.output.clone_from(&child.output);
        result.own = child.own;
        let rows = child.actual.map(|actual| actual.rows);
        result.children.push(Child::new("Outer", child));
        self.time(&mut result, rows, 0, false);
        result
    }

    /// The node for a plan node, without a projection.
    fn raw(&self, at: NodeRef) -> Shown {
        let plan = self.plan;
        match *plan.node(at) {
            Node::Project { input, .. } => self.node(input, Some(at)),
            Node::Filter { input, predicate } => {
                let shown = self.node(input, None);
                self.filtered(shown, predicate, at)
            }
            Node::Get { catalog, schema, table, index, columns, .. } => self.seq_scan(
                at,
                plan.string(catalog),
                plan.string(schema),
                plan.string(table),
                index,
                columns,
            ),
            Node::Dummy => {
                let mut shown = Shown::new("Result", "Result", Quals::Upper);
                shown.cost = Cost { startup: 0.0, total: CPU_TUPLE_COST, rows: 1.0, width: 0.0 };
                self.measure(&mut shown, at, false);
                shown
            }
            Node::Values { index, rows, .. } => {
                let target = self.target(None, None, None, index);
                let mut shown = Shown::scan("Values Scan", target, index);
                #[expect(clippy::cast_precision_loss, reason = "a VALUES list is short")]
                let count = plan.row_list(rows).len() as f64;
                shown.cost.total = count * (CPU_OPERATOR_COST + CPU_TUPLE_COST);
                shown.cost.rows = clamp(count);
                self.natural(&mut shown, at);
                self.measure(&mut shown, at, false);
                shown
            }
            Node::TableFunction { index, function, args, .. } => {
                let mut shown = self.function_scan(index, plan.string(function), args);
                self.natural(&mut shown, at);
                self.measure(&mut shown, at, false);
                shown
            }
            Node::LateralFunction { input, index, function, args, columns, .. } => {
                let outer = self.node(input, None);
                let mut inner = self.function_scan(index, plan.string(function), args);
                inner.cost.width = plan.field_list(columns).iter().map(|f| width(&f.ty)).sum();
                if self.options.verbose {
                    let deparse = self.deparse(self.output_prefix(), Some(index));
                    inner.output = outputs(plan, at)
                        .iter()
                        .filter(|(column, _)| column.table == index)
                        .map(|&(column, _)| deparse.column(column))
                        .collect();
                }
                let operator = self.operator(at);
                self.time(&mut inner, operator.map(|o| o.rows_out), 0, false);
                let rows = clamp(outer.cost.rows * inner.cost.rows);
                let mut shown = self.nested_loop(outer, inner, "Inner", rows, Vec::new());
                self.natural(&mut shown, at);
                self.measure(&mut shown, at, false);
                shown
            }
            Node::Aggregate { input, index, groups, aggregates } => {
                let child = self.node(input, None);
                let groups = plan.expr_list(groups);
                let aggregates = plan.expr_list(aggregates);
                let evaluated: f64 =
                    aggregates.iter().map(|&aggregate| 1.0 + self.operators(aggregate)).sum();
                let mut shown;
                if groups.is_empty() {
                    shown = Shown::new("Aggregate", "Aggregate", Quals::Upper);
                    shown.strategy = Some("Plain");
                    let startup =
                        child.cost.total + child.cost.rows * CPU_OPERATOR_COST * evaluated;
                    shown.cost =
                        Cost { startup, total: startup + CPU_TUPLE_COST, rows: 1.0, width: 0.0 };
                } else {
                    shown = self.hash_aggregate(&child, groups.len(), evaluated);
                    let keys = self.deparse(self.upper_prefix(), None).exprs(groups);
                    shown.keys.push(Prop::List("Group Key", keys));
                    shown.info.push(Prop::HashAgg { kb: self.ran(at) });
                }
                shown.partial = true;
                shown.removed_last = true;
                shown.own = Some(index);
                shown.children.push(Child::new("Outer", child));
                self.natural(&mut shown, at);
                self.measure(&mut shown, at, true);
                shown
            }
            Node::Window { input, index, partition, order, frame, expressions } => {
                let mut child = self.node(input, None);
                let mut shown = Shown::new("WindowAgg", "WindowAgg", Quals::Upper);
                let partition = plan.expr_list(partition);
                let order = plan.sort_key_list(order);
                // The window lays its rows out itself, and PostgreSQL plans that as a `Sort` on
                // the partition and then the order under the `WindowAgg`.
                if !partition.is_empty() || !order.is_empty() {
                    let deparse = self.deparse(self.upper_prefix(), None);
                    let mut keys = deparse.exprs(partition);
                    keys.extend(order.iter().map(|key| sort_key(&deparse, key)));
                    child = self.sort(child, keys, None, at, false);
                }
                #[expect(clippy::cast_precision_loss, reason = "a key list is short")]
                let per_row =
                    (plan.expr_list(expressions).len() + partition.len() + order.len()) as f64;
                let total = child.cost.total
                    + child.cost.rows * (CPU_OPERATOR_COST * per_row + CPU_TUPLE_COST);
                let rows = child.cost.rows;
                shown.cost = Cost {
                    startup: child.cost.startup + (total - child.cost.startup) / rows.max(1.0),
                    total,
                    rows,
                    width: 0.0,
                };
                let window = self.window(at, partition, order, frame, expressions);
                shown.keys.push(Prop::Text("Window", window));
                if let Some(kb) = self.ran(at) {
                    shown.info.push(Prop::Storage { kb });
                }
                shown.own = Some(index);
                shown.children.push(Child::new("Outer", child));
                self.natural(&mut shown, at);
                self.measure(&mut shown, at, true);
                shown
            }
            Node::Sort { input, keys } => {
                let child = self.node(input, None);
                let keys = self.sort_keys(plan.sort_key_list(keys));
                let mut shown = self.sort(child, keys, None, at, true);
                self.natural(&mut shown, at);
                shown
            }
            Node::TopN { input, keys, count, offset } => {
                let child = self.node(input, None);
                #[expect(clippy::cast_precision_loss, reason = "a limit is far below 2^52")]
                let wanted = (count + offset) as f64;
                let keys = self.sort_keys(plan.sort_key_list(keys));
                let mut sort = self.sort(child, keys, Some(wanted), at, true);
                self.natural(&mut sort, at);
                #[expect(clippy::cast_precision_loss, reason = "a limit is far below 2^52")]
                let (count, offset) = (count as f64, offset as f64);
                self.limit(sort, Some(count), offset, at)
            }
            Node::Limit { input, count, offset } => {
                let child = self.node(input, None);
                let rows = child.cost.rows;
                let count = self.bound(count, rows);
                let offset = self.bound(offset, rows).unwrap_or(0.0);
                self.limit(child, count, offset, at)
            }
            // PostgreSQL shows `WITH TIES` as a plain limit.
            Node::LimitTies { input, count, offset, .. } => {
                let child = self.node(input, None);
                let rows = child.cost.rows;
                let count = self.bound(count, rows);
                let offset = self.bound(offset, rows).unwrap_or(0.0);
                self.limit(child, count, offset, at)
            }
            Node::LimitPercent { input, percent, offset } => {
                let child = self.node(input, None);
                let rows = child.cost.rows;
                let share = percent.percent().unwrap_or(10.0) / 100.0;
                let offset = self.bound(offset, rows).unwrap_or(0.0);
                let mut shown = self.limit(child, Some(rows * share), offset, at);
                shown.rudb = Some("Limit Percent");
                shown
            }
            Node::Distinct { input, on } => {
                let child = self.node(input, None);
                let on = plan.expr_list(on);
                let mut shown;
                if on.is_empty() {
                    let columns = outputs(plan, input);
                    shown = self.hash_aggregate(&child, columns.len(), 0.0);
                    let deparse = self.deparse(self.upper_prefix(), None);
                    let keys = columns.iter().map(|&(column, _)| deparse.column(column)).collect();
                    shown.keys.push(Prop::List("Group Key", keys));
                    shown.info.push(Prop::HashAgg { kb: self.ran(at) });
                    shown.partial = true;
                } else {
                    shown = Shown::new("Unique", "Unique", Quals::None);
                    shown.rudb = Some("Distinct On");
                    #[expect(clippy::cast_precision_loss, reason = "a key list is short")]
                    let keys = on.len() as f64;
                    shown.cost = Cost {
                        startup: child.cost.startup,
                        total: child.cost.total + child.cost.rows * CPU_OPERATOR_COST * keys,
                        rows: groups(child.cost.rows, on.len()),
                        width: 0.0,
                    };
                }
                shown.own = child.own;
                shown.children.push(Child::new("Outer", child));
                self.natural(&mut shown, at);
                self.measure(&mut shown, at, on.is_empty());
                shown
            }
            Node::Join { left, right, kind, conditions, build } => {
                self.join(at, left, right, kind, plan.expr_list(conditions), build)
            }
            Node::DependentJoin { left, right, kind, conditions } => {
                let mut shown = self.loop_join(at, left, right, kind, plan.expr_list(conditions));
                shown.rudb = Some("Dependent Join");
                shown
            }
            Node::LinkJoin { child, parent, kind, conditions, .. } => {
                let mut shown = self.loop_join(at, child, parent, kind, plan.expr_list(conditions));
                shown.rudb = Some("Link Join");
                shown
            }
            Node::CrossProduct { left, right } => {
                self.loop_join(at, left, right, JoinKind::Inner, &[])
            }
            Node::MaterializedCte { definition, body, name, cte, .. } => {
                // A recursive definition is its union, which is what the `CTE` subplan of
                // PostgreSQL holds.
                let definition = match plan.node(definition) {
                    Node::RecursiveCte { .. } => self.recursive_union(definition),
                    _ => self.node(definition, None),
                };
                self.ctes.borrow_mut().insert(cte, definition.cost.rows);
                let mut shown = self.node(body, None);
                shown.cost.startup += definition.cost.total;
                shown.cost.total += definition.cost.total;
                let mut child = Child::new("InitPlan", definition);
                child.plan_name = Some(format!("CTE {}", plan.string(name)));
                shown.children.insert(0, child);
                shown
            }
            Node::CteScan { index, cte, name, .. } => {
                let anchor = self
                    .recursive
                    .borrow()
                    .iter()
                    .find(|(id, _)| *id == cte)
                    .map(|&(_, rows)| rows);
                let target =
                    self.target(Some("CTE Name"), Some(plan.string(name).to_owned()), None, index);
                let mut shown;
                match anchor {
                    Some(rows) => {
                        shown = Shown::scan("WorkTable Scan", target, index);
                        shown.cost.rows = clamp(rows * RECURSIVE_ROUNDS);
                    }
                    None => {
                        shown = Shown::scan("CTE Scan", target, index);
                        shown.cost.rows =
                            self.ctes.borrow().get(&cte).copied().unwrap_or(DEFAULT_FUNCTION_ROWS);
                        if let Some(kb) = self.ran(at) {
                            shown.info.push(Prop::Storage { kb });
                        }
                    }
                }
                shown.cost.total = shown.cost.rows * CPU_TUPLE_COST;
                self.natural(&mut shown, at);
                self.measure(&mut shown, at, false);
                shown
            }
            Node::RecursiveCte { index, name, .. } => {
                let union = self.recursive_union(at);
                let target =
                    self.target(Some("CTE Name"), Some(plan.string(name).to_owned()), None, index);
                let mut shown = Shown::scan("CTE Scan", target, index);
                shown.cost = Cost {
                    startup: union.cost.total,
                    total: union.cost.total + union.cost.rows * CPU_TUPLE_COST,
                    rows: union.cost.rows,
                    width: 0.0,
                };
                self.natural(&mut shown, at);
                let mut child = Child::new("InitPlan", union);
                child.plan_name = Some(format!("CTE {}", plan.string(name)));
                shown.children.push(child);
                self.measure(&mut shown, at, false);
                shown
            }
            Node::SetOp { left, right, kind, all, index } => {
                self.set_operation(at, left, right, kind, all, index)
            }
            Node::Fetch { input, index, .. } => {
                let child = self.node(input, None);
                let target = self.target(None, None, None, index);
                self.fetch(at, child, target, "Fetch")
            }
            Node::TableFetch { input, index, table, .. } => {
                let child = self.node(input, None);
                let target = self.target(
                    Some("Relation Name"),
                    Some(plan.string(table).to_owned()),
                    None,
                    index,
                );
                self.fetch(at, child, target, "Table Fetch")
            }
            Node::Consistent { index, .. } => {
                let mut shown = Shown::new("Nested Loop", "Nested Loop", Quals::Upper);
                shown.join = Some("Inner");
                shown.unique = Some(false);
                shown.rudb = Some("Consistent");
                shown.own = Some(index);
                let members: Vec<Shown> =
                    inputs(plan, at).into_iter().map(|leaf| self.node(leaf, None)).collect();
                let rows = estimated(rows_stat(plan, at, self.facts));
                let below: f64 = members.iter().map(|member| member.cost.total).sum();
                shown.cost =
                    Cost { startup: 0.0, total: below + rows * CPU_TUPLE_COST, rows, width: 0.0 };
                shown.children =
                    members.into_iter().map(|member| Child::new("Member", member)).collect();
                self.natural(&mut shown, at);
                self.measure(&mut shown, at, false);
                shown
            }
        }
    }

    /// The `Recursive Union` of a recursive CTE, as `cost_recursive_union` costs it: ten rounds of
    /// the recursive part.
    fn recursive_union(&self, at: NodeRef) -> Shown {
        let Node::RecursiveCte { anchor, recursive, cte, key, .. } = *self.plan.node(at) else {
            unreachable!("only a recursive CTE has a recursive union")
        };
        let anchor = self.node(anchor, None);
        self.recursive.borrow_mut().push((cte, anchor.cost.rows));
        let recursive = self.node(recursive, None);
        self.recursive.borrow_mut().pop();
        let mut union = Shown::new("Recursive Union", "Recursive Union", Quals::None);
        if !self.plan.expr_list(key).is_empty() {
            union.rudb = Some("Recursive Union Using Key");
        }
        union.cost = Cost {
            startup: anchor.cost.startup,
            total: anchor.cost.total + RECURSIVE_ROUNDS * recursive.cost.total,
            rows: clamp(anchor.cost.rows + RECURSIVE_ROUNDS * recursive.cost.rows),
            width: anchor.cost.width,
        };
        if let Some(kb) = self.ran(at) {
            union.info.push(Prop::Storage { kb });
        }
        union.children.push(Child::new("Outer", anchor));
        union.children.push(Child::new("Inner", recursive));
        self.measure(&mut union, at, false);
        union
    }

    /// A scan of a table, with the rows PostgreSQL guesses for a table it has not analyzed:
    /// `table_block_relation_estimate_size` with no `relpages`.
    fn seq_scan(
        &self,
        at: NodeRef,
        catalog: &str,
        schema: &str,
        table: &str,
        index: u32,
        columns: rudb_plan::Slice,
    ) -> Shown {
        let read: Vec<LogicalType> =
            self.plan.field_list(columns).iter().map(|field| field.ty.clone()).collect();
        let all = (self.table)(catalog, schema, table).unwrap_or_else(|| read.clone());
        let row: f64 = all.iter().map(width).sum();
        let density = (USABLE_BYTES_PER_PAGE / (row + OVERHEAD_BYTES_PER_TUPLE)).floor().max(1.0);
        let counted = estimated_or(rows_stat(self.plan, at, self.facts), 0.0);
        let pages = (counted / density).ceil().max(UNANALYZED_PAGES);
        let rows = clamp(density * pages);
        let namespace = match catalog {
            "temp" => "pg_temp",
            _ => schema,
        };
        let schema = self.options.verbose.then(|| namespace.to_owned());
        let target = self.target(Some("Relation Name"), Some(table.to_owned()), schema, index);
        let mut shown = Shown::scan("Seq Scan", target, index);
        shown.cost = Cost {
            startup: 0.0,
            total: pages * SEQ_PAGE_COST + rows * CPU_TUPLE_COST,
            rows,
            width: read.iter().map(width).sum(),
        };
        self.natural(&mut shown, at);
        self.measure(&mut shown, at, false);
        shown
    }

    /// A scan of a function. `generate_series` over integer constants has the rows its support
    /// function counts, and any other function has its `prorows`.
    fn function_scan(&self, index: u32, function: &str, args: rudb_plan::Slice) -> Shown {
        let name = function_name(function);
        let schema = self.options.verbose.then(|| "pg_catalog".to_owned());
        let target = self.target(Some("Function Name"), Some(name.to_owned()), schema, index);
        let mut shown = Shown::scan("Function Scan", target, index);
        let args = self.plan.expr_list(args);
        let rows = match name {
            "generate_series" => self.series(args).unwrap_or(DEFAULT_FUNCTION_ROWS),
            _ => DEFAULT_FUNCTION_ROWS,
        };
        let startup: f64 = CPU_OPERATOR_COST
            + args.iter().map(|&arg| self.operators(arg)).sum::<f64>() * CPU_OPERATOR_COST;
        shown.cost = Cost { startup, total: startup + rows * CPU_TUPLE_COST, rows, width: 0.0 };
        if self.options.verbose {
            let deparse = self.deparse(true, None);
            let call = format!("{}({})", quote_identifier(name), deparse.exprs(args).join(", "));
            shown.keys.push(Prop::Text("Function Call", call));
        }
        shown
    }

    /// The rows of `generate_series(start, stop[, step])` over integer constants.
    #[expect(clippy::cast_precision_loss, reason = "an estimate")]
    fn series(&self, args: &[ExprRef]) -> Option<f64> {
        let integer = |at: ExprRef| match self.plan.expr(at) {
            Expr::Constant(value) => self.plan.value(*value).as_i64(),
            Expr::Cast { input, .. } => match self.plan.expr(*input) {
                Expr::Constant(value) => self.plan.value(*value).as_i64(),
                _ => None,
            },
            _ => None,
        };
        let (start, stop, step) = match *args {
            [start, stop] => (integer(start)?, integer(stop)?, 1),
            [start, stop, step] => (integer(start)?, integer(stop)?, integer(step)?),
            _ => return None,
        };
        if step == 0 {
            return None;
        }
        let count = (i128::from(stop) - i128::from(start)) / i128::from(step) + 1;
        Some(clamp(count.max(0) as f64))
    }

    fn target(
        &self,
        tag: Option<&'static str>,
        object: Option<String>,
        schema: Option<String>,
        index: u32,
    ) -> Target {
        let refname = self
            .names
            .refname(index)
            .map_or_else(|| object.clone().unwrap_or_default(), str::to_owned);
        Target { tag, object, schema, refname }
    }

    /// A filter, which goes on the node under it when that node takes one.
    fn filtered(&self, mut shown: Shown, predicate: ExprRef, at: NodeRef) -> Shown {
        if shown.quals == Quals::None || shown.filter.is_some() {
            shown = self.result_over(shown);
        }
        let prefix = match shown.quals {
            Quals::Scan => self.options.verbose,
            _ => self.upper_prefix(),
        };
        shown.filter = Some(self.qual(prefix, shown.own, predicate));
        let rows = shown.cost.rows;
        shown.cost.total += rows * self.operators(predicate) * CPU_OPERATOR_COST;
        shown.cost.rows = clamp(rows * self.selectivity(predicate));
        if !self.analyze() {
            return shown;
        }
        match (self.operator(at), shown.actual.as_mut()) {
            (Some(operator), Some(actual)) => {
                shown.removed = Some(operator.rows_in.saturating_sub(operator.rows_out));
                actual.rows = operator.rows_out;
                actual.total_ns += operator.wall_ns;
            }
            // A filter the scan applies itself has no operator of its own, and the scan's rows are
            // the rows after it.
            (None, Some(actual)) => {
                shown.removed =
                    Some(shown.rows_in.map_or(0, |rows| rows.saturating_sub(actual.rows)));
            }
            (_, None) => shown.removed = Some(0),
        }
        shown
    }

    /// A hash aggregate over a node, as `cost_agg` costs `AGG_HASHED`.
    fn hash_aggregate(&self, child: &Shown, keys: usize, evaluated: f64) -> Shown {
        let mut shown = Shown::new("Aggregate", "HashAggregate", Quals::Upper);
        shown.strategy = Some("Hashed");
        #[expect(clippy::cast_precision_loss, reason = "a key list is short")]
        let per_row = keys as f64 + evaluated;
        let rows = groups(child.cost.rows, keys);
        let startup = child.cost.total + child.cost.rows * CPU_OPERATOR_COST * per_row;
        shown.cost = Cost { startup, total: startup + rows * CPU_TUPLE_COST, rows, width: 0.0 };
        shown
    }

    /// A sort, bounded when it keeps only the first rows, as `cost_sort` costs it.
    ///
    /// A sort is an operator of its own when `apart` is set. Otherwise it is the sort the operator
    /// `at` does inside, which PostgreSQL plans as a `Sort` under it, and it gets the rows it was
    /// given and the memory of that operator.
    fn sort(
        &self,
        child: Shown,
        keys: Vec<String>,
        bound: Option<f64>,
        at: NodeRef,
        apart: bool,
    ) -> Shown {
        let mut shown = Shown::new("Sort", "Sort", Quals::None);
        let rows = child.cost.rows;
        let comparison = 2.0 * CPU_OPERATOR_COST;
        let compared = match bound {
            Some(wanted) if rows > 2.0 * wanted => rows * log2(2.0 * wanted),
            _ => rows * log2(rows),
        };
        let startup = child.cost.total + comparison * compared;
        shown.cost = Cost {
            startup,
            total: startup + CPU_OPERATOR_COST * rows,
            rows,
            width: child.cost.width,
        };
        shown.output.clone_from(&child.output);
        shown.keys.push(Prop::List("Sort Key", keys));
        if let Some(kb) = self.ran(at) {
            let method = if bound.is_some() { "top-N heapsort" } else { "quicksort" };
            shown.info.push(Prop::Sort { method, kb });
        }
        shown.own = child.own;
        let given = child.actual.map(|actual| actual.rows);
        shown.children.push(Child::new("Outer", child));
        match apart {
            true => self.measure(&mut shown, at, true),
            false => self.time(&mut shown, given, 0, true),
        }
        shown
    }

    /// The keys of a sort as `show_sort_keys` writes them.
    fn sort_keys(&self, keys: &[SortKey]) -> Vec<String> {
        let deparse = self.deparse(self.upper_prefix(), None);
        keys.iter().map(|key| sort_key(&deparse, key)).collect()
    }

    /// The memory of a node that ran, under `ANALYZE`.
    fn ran(&self, at: NodeRef) -> Option<u64> {
        self.operator(at).map(|_| self.memory_kb(at))
    }

    /// A limit, as `adjust_limit_rows_costs` costs it.
    fn limit(&self, child: Shown, count: Option<f64>, offset: f64, at: NodeRef) -> Shown {
        let mut shown = Shown::new("Limit", "Limit", Quals::None);
        let input = child.cost;
        let rows = input.rows.max(1.0);
        let run = input.total - input.startup;
        let offset = offset.min(rows);
        let startup = input.startup + run * offset / rows;
        let wanted = count.map_or(rows - offset, |count| count.min(rows - offset));
        shown.cost = Cost {
            startup,
            total: startup + run * wanted / rows,
            rows: clamp(wanted),
            width: input.width,
        };
        shown.output.clone_from(&child.output);
        shown.own = child.own;
        shown.children.push(Child::new("Outer", child));
        self.measure(&mut shown, at, false);
        shown
    }

    /// The rows a limit's bound keeps: a count, every row, or a tenth of them for a bound read from
    /// an expression, which is what `limit_needed` guesses.
    fn bound(&self, bound: Bound, rows: f64) -> Option<f64> {
        match bound {
            Bound::All => None,
            #[expect(clippy::cast_precision_loss, reason = "a limit is far below 2^52")]
            Bound::Rows(count) => Some(count as f64),
            Bound::Read(_) => Some(clamp(rows * 0.1)),
        }
    }

    /// `show_window_def`.
    fn window(
        &self,
        at: NodeRef,
        partition: &[ExprRef],
        order: &[SortKey],
        frame: WindowFrame,
        expressions: rudb_plan::Slice,
    ) -> String {
        let deparse = self.deparse(self.upper_prefix(), None);
        let mut parts = Vec::new();
        if !partition.is_empty() {
            parts.push(format!("PARTITION BY {}", deparse.exprs(partition).join(", ")));
        }
        if !order.is_empty() {
            let keys: Vec<String> = order.iter().map(|key| sort_key(&deparse, key)).collect();
            parts.push(format!("ORDER BY {}", keys.join(", ")));
        }
        // `optimize_window_clauses` turns the frame of a window whose functions all only count
        // rows into `ROWS UNBOUNDED PRECEDING`, since the support function of each says the frame
        // does not change its result.
        let counting = ["row_number", "rank", "dense_rank", "percent_rank", "cume_dist", "ntile"];
        let calls = self.plan.expr_list(expressions);
        let counts = !calls.is_empty()
            && calls.iter().all(|&call| {
                matches!(self.plan.expr(call), Expr::Window { name, .. }
                    if counting.contains(&function_name(self.plan.string(*name))))
            });
        let frame = match counts {
            true => WindowFrame {
                unit: WindowUnit::Rows,
                start: WindowBound::UnboundedPreceding,
                end: WindowBound::CurrentRow,
                exclude: WindowExclude::NoOthers,
            },
            false => frame,
        };
        let default = !counts
            && frame.unit == WindowUnit::Range
            && frame.start == WindowBound::UnboundedPreceding
            && frame.end == WindowBound::CurrentRow
            && frame.exclude == WindowExclude::NoOthers;
        if !default {
            parts.push(frame_text(&deparse, frame));
        }
        format!("{} AS ({})", quote_identifier(self.names.window(at)), parts.join(" "))
    }

    /// A join, which is a hash join when it has an equality between its two sides and a nested
    /// loop when it has not.
    fn join(
        &self,
        at: NodeRef,
        left: NodeRef,
        right: NodeRef,
        kind: JoinKind,
        conditions: &[ExprRef],
        build: BuildSide,
    ) -> Shown {
        let (outer_at, inner_at, mirrored) = match build {
            BuildSide::Right => (left, right, false),
            BuildSide::Left => (right, left, true),
        };
        let outer_tables = tables(self.plan, outer_at);
        let inner_tables = tables(self.plan, inner_at);
        let mut hashed = Vec::new();
        let mut rest = Vec::new();
        for &condition in conditions {
            match self.sides(condition, &outer_tables, &inner_tables) {
                Some(pair) => hashed.push(pair),
                None => rest.push(condition),
            }
        }
        if hashed.is_empty() {
            return self.loop_join(at, left, right, kind, conditions);
        }
        let (name, rudb) = join_type(kind, mirrored);
        let outer = self.node(outer_at, None);
        let inner = self.node(inner_at, None);
        let mut hash = Shown::new("Hash", "Hash", Quals::None);
        hash.cost = Cost {
            startup: inner.cost.total,
            total: inner.cost.total,
            rows: inner.cost.rows,
            width: inner.cost.width,
        };
        hash.output.clone_from(&inner.output);
        hash.own = inner.own;
        let gathered = self
            .shape
            .operator_of(at)
            .and_then(|_| self.shape.gathered(at))
            .and_then(|id| self.find(id));
        if let Some(operator) = gathered {
            let buckets = inner.cost.rows.max(1024.0);
            #[expect(
                clippy::cast_possible_truncation,
                clippy::cast_sign_loss,
                reason = "a bucket count"
            )]
            let buckets = (buckets as u64).next_power_of_two();
            hash.info.push(Prop::Hash { buckets, kb: kilobytes(operator.memory.high_water) });
        }
        let inner_rows = inner.actual.map(|actual| actual.rows);
        hash.children.push(Child::new("Outer", inner));
        self.time(&mut hash, inner_rows, gathered.map_or(0, |operator| operator.wall_ns), true);

        let deparse = self.deparse(self.upper_prefix(), None);
        let conds: Vec<String> = hashed
            .iter()
            .map(|&(outer_side, inner_side)| {
                format!("({} = {})", deparse.expr(outer_side), deparse.expr(inner_side))
            })
            .collect();
        let mut shown = Shown::new("Hash Join", "Hash", Quals::Upper);
        shown.join = Some(name);
        shown.rudb = rudb;
        shown.unique = Some(false);
        shown.keys.push(Prop::Text("Hash Cond", anded(conds)));
        self.join_filter(&mut shown, &rest);

        let mut selectivity = 1.0;
        for _ in &hashed {
            let distinct = distinct(outer.cost.rows).max(distinct(hash.cost.rows));
            selectivity /= distinct;
        }
        for &condition in &rest {
            selectivity *= self.selectivity(condition);
        }
        let rows = join_rows(kind, mirrored, outer.cost.rows, hash.cost.rows, selectivity);
        #[expect(clippy::cast_precision_loss, reason = "a condition list is short")]
        let clauses = hashed.len() as f64;
        let quals: f64 = rest.iter().map(|&condition| self.operators(condition)).sum();
        let startup = outer.cost.startup
            + hash.cost.total
            + hash.cost.rows * (CPU_OPERATOR_COST * clauses + CPU_TUPLE_COST);
        let total = startup
            + (outer.cost.total - outer.cost.startup)
            + outer.cost.rows * CPU_OPERATOR_COST * clauses
            + rows * (CPU_TUPLE_COST + quals * CPU_OPERATOR_COST);
        shown.cost = Cost { startup, total, rows, width: 0.0 };
        shown.children.push(Child::new("Outer", outer));
        shown.children.push(Child::new("Inner", hash));
        self.natural(&mut shown, at);
        self.measure(&mut shown, at, false);
        shown
    }

    /// A join with no equality to hash, which is a nested loop.
    fn loop_join(
        &self,
        at: NodeRef,
        left: NodeRef,
        right: NodeRef,
        kind: JoinKind,
        conditions: &[ExprRef],
    ) -> Shown {
        let outer = self.node(left, None);
        let inner = self.node(right, None);
        let selectivity: f64 =
            conditions.iter().map(|&condition| self.selectivity(condition)).product();
        let rows = join_rows(kind, false, outer.cost.rows, inner.cost.rows, selectivity);
        let (name, rudb) = join_type(kind, false);
        let mut shown = self.nested_loop(outer, inner, name, rows, conditions.to_vec());
        shown.rudb = rudb;
        self.natural(&mut shown, at);
        self.measure(&mut shown, at, false);
        shown
    }

    /// A nested loop, as `final_cost_nestloop` costs it.
    fn nested_loop(
        &self,
        outer: Shown,
        inner: Shown,
        name: &'static str,
        rows: f64,
        conditions: Vec<ExprRef>,
    ) -> Shown {
        let mut shown = Shown::new("Nested Loop", "Nested Loop", Quals::Upper);
        shown.join = Some(name);
        shown.unique = Some(false);
        let quals: f64 = conditions.iter().map(|&condition| self.operators(condition)).sum();
        let pairs = outer.cost.rows * inner.cost.rows;
        shown.cost = Cost {
            startup: outer.cost.startup + inner.cost.startup,
            total: outer.cost.total
                + outer.cost.rows * (inner.cost.total - inner.cost.startup)
                + inner.cost.startup
                + pairs * (CPU_TUPLE_COST + quals * CPU_OPERATOR_COST),
            rows,
            width: 0.0,
        };
        self.join_filter(&mut shown, &conditions);
        shown.children.push(Child::new("Outer", outer));
        shown.children.push(Child::new("Inner", inner));
        shown
    }

    /// The conditions of a join that are not hashed, with the rows they removed under `ANALYZE`,
    /// which rudb does not count apart from the join's.
    fn join_filter(&self, shown: &mut Shown, conditions: &[ExprRef]) {
        if conditions.is_empty() {
            return;
        }
        let deparse = self.deparse(self.upper_prefix(), None);
        shown.keys.push(Prop::Text("Join Filter", anded(deparse.exprs(conditions))));
        if self.analyze() {
            shown.keys.push(Prop::Removed("Rows Removed by Join Filter", 0));
        }
    }

    /// The two operands of an equality between the outer and the inner side, outer first.
    fn sides(
        &self,
        condition: ExprRef,
        outer: &HashSet<u32>,
        inner: &HashSet<u32>,
    ) -> Option<(ExprRef, ExprRef)> {
        let Expr::Compare { op: CompareOp::Equal, left, right } = *self.plan.expr(condition) else {
            return None;
        };
        let reads = |at: ExprRef| {
            let mut found = HashSet::new();
            self.plan.read_columns(at, &mut |_, column| {
                found.insert(column.table);
            });
            found
        };
        let (on_left, on_right) = (reads(left), reads(right));
        if on_left.is_empty() || on_right.is_empty() {
            return None;
        }
        if on_left.is_subset(outer) && on_right.is_subset(inner) {
            Some((left, right))
        } else if on_left.is_subset(inner) && on_right.is_subset(outer) {
            Some((right, left))
        } else {
            None
        }
    }

    /// A set operation. `UNION ALL` is an `Append` of its inputs, `UNION` is a `HashAggregate`
    /// over one, and `INTERSECT` and `EXCEPT` are a `HashSetOp` over the two inputs, which is what
    /// PostgreSQL 18 plans.
    fn set_operation(
        &self,
        at: NodeRef,
        left: NodeRef,
        right: NodeRef,
        kind: SetOpKind,
        all: bool,
        index: u32,
    ) -> Shown {
        if kind == SetOpKind::Union {
            let mut members = Vec::new();
            self.members(left, all, &mut members);
            self.members(right, all, &mut members);
            let members: Vec<Shown> =
                members.into_iter().map(|member| self.node(member, None)).collect();
            let mut append = Shown::new("Append", "Append", Quals::None);
            let rows: f64 = members.iter().map(|member| member.cost.rows).sum();
            append.cost = Cost {
                startup: members.first().map_or(0.0, |member| member.cost.startup),
                total: members.iter().map(|member| member.cost.total).sum::<f64>()
                    + rows * CPU_TUPLE_COST * 0.5,
                rows,
                width: members.first().map_or(0.0, |member| member.cost.width),
            };
            let measured: Option<u64> =
                members.iter().map(|member| member.actual.map(|actual| actual.rows)).sum();
            append.children =
                members.into_iter().map(|member| Child::new("Member", member)).collect();
            append.own = Some(index);
            if all {
                self.measure(&mut append, at, false);
                return append;
            }
            self.time(&mut append, measured, 0, false);
            let columns = outputs(self.plan, at);
            let mut shown = self.hash_aggregate(&append, columns.len(), 0.0);
            let deparse = self.deparse(self.upper_prefix(), None);
            let keys = columns.iter().map(|&(column, _)| deparse.column(column)).collect();
            shown.keys.push(Prop::List("Group Key", keys));
            shown.info.push(Prop::HashAgg { kb: self.ran(at) });
            shown.partial = true;
            shown.own = Some(index);
            shown.children.push(Child::new("Outer", append));
            self.natural(&mut shown, at);
            self.measure(&mut shown, at, true);
            return shown;
        }
        let outer = self.node(left, None);
        let inner = self.node(right, None);
        let mut shown = Shown::new("SetOp", "HashSetOp", Quals::None);
        shown.strategy = Some("Hashed");
        shown.command = Some(match (kind, all) {
            (SetOpKind::Intersect, false) => "Intersect",
            (SetOpKind::Intersect, true) => "Intersect All",
            (_, false) => "Except",
            (_, true) => "Except All",
        });
        let columns = outputs(self.plan, at).len();
        let rows = match kind {
            SetOpKind::Intersect => {
                groups(outer.cost.rows, columns).min(groups(inner.cost.rows, columns))
            }
            _ => groups(outer.cost.rows, columns),
        };
        #[expect(clippy::cast_precision_loss, reason = "a column list is short")]
        let per_row = columns as f64;
        let startup = outer.cost.total
            + inner.cost.total
            + (outer.cost.rows + inner.cost.rows) * CPU_OPERATOR_COST * per_row;
        shown.cost = Cost { startup, total: startup + rows * CPU_TUPLE_COST, rows, width: 0.0 };
        shown.own = Some(index);
        shown.children.push(Child::new("Outer", outer));
        shown.children.push(Child::new("Inner", inner));
        self.natural(&mut shown, at);
        self.measure(&mut shown, at, true);
        shown
    }

    /// The inputs of a tree of unions of one kind, which `flatten_simple_union_all` and the union
    /// planner make one list of.
    fn members(&self, at: NodeRef, all: bool, found: &mut Vec<NodeRef>) {
        match *self.plan.node(at) {
            Node::SetOp { left, right, kind: SetOpKind::Union, all: same, .. } if same == all => {
                self.members(left, all, found);
                self.members(right, all, found);
            }
            _ => found.push(at),
        }
    }

    /// A fetch of rows by their ids, which is closest to a `Tid Scan`.
    fn fetch(&self, at: NodeRef, child: Shown, target: Target, rudb: &'static str) -> Shown {
        let mut shown = Shown::new("Tid Scan", "Tid Scan", Quals::Scan);
        shown.target = Some(target);
        shown.rudb = Some(rudb);
        shown.own = self.plan.node(at).table_index();
        shown.cost = Cost {
            startup: child.cost.startup,
            total: child.cost.total + child.cost.rows * (SEQ_PAGE_COST + CPU_TUPLE_COST),
            rows: child.cost.rows,
            width: 0.0,
        };
        shown.children.push(Child::new("Outer", child));
        self.natural(&mut shown, at);
        self.measure(&mut shown, at, false);
        shown
    }

    /// The operators an expression runs, each of which costs `cpu_operator_cost` a row.
    fn operators(&self, at: ExprRef) -> f64 {
        let own = match self.plan.expr(at) {
            // `eval_const_expressions` folds a cast of a constant before anything is costed.
            Expr::Cast { input, .. } if matches!(self.plan.expr(*input), Expr::Constant(_)) => {
                return 0.0;
            }
            // `IS NULL` is a `NullTest` and `IS TRUE` a `BooleanTest`, which cost nothing.
            Expr::Compare {
                op: CompareOp::DistinctFrom | CompareOp::NotDistinctFrom,
                right,
                ..
            } if matches!(
                self.plan.expr(*right),
                Expr::Constant(value)
                    if matches!(self.plan.value(*value), Value::Null | Value::Boolean(_))
            ) =>
            {
                0.0
            }
            Expr::Compare { .. } | Expr::Function { .. } | Expr::Cast { .. } => 1.0,
            _ => 0.0,
        };
        let mut count = own;
        self.plan.for_each_operand(at, &mut |operand| count += self.operators(operand));
        count
    }

    /// The share of rows a condition keeps, with the defaults of `selfuncs.h` for columns nobody
    /// has statistics for.
    fn selectivity(&self, at: ExprRef) -> f64 {
        const DEFAULT_EQ_SEL: f64 = 0.005;
        const DEFAULT_INEQ_SEL: f64 = 1.0 / 3.0;
        match self.plan.expr(at) {
            Expr::Conjunction { op: ConjunctionOp::And, children } => self
                .plan
                .expr_list(*children)
                .iter()
                .map(|&child| self.selectivity(child))
                .product(),
            Expr::Conjunction { op: ConjunctionOp::Or, children } => {
                self.plan.expr_list(*children).iter().fold(0.0, |kept, &child| {
                    let more = self.selectivity(child);
                    kept + more - kept * more
                })
            }
            Expr::Compare { op, right, .. } => {
                let null = matches!(self.plan.expr(*right), Expr::Constant(value) if self.plan.value(*value).is_null());
                match op {
                    CompareOp::Equal => DEFAULT_EQ_SEL,
                    CompareOp::NotEqual => 1.0 - DEFAULT_EQ_SEL,
                    CompareOp::NotDistinctFrom if null => DEFAULT_EQ_SEL,
                    CompareOp::DistinctFrom if null => 1.0 - DEFAULT_EQ_SEL,
                    CompareOp::NotDistinctFrom => DEFAULT_EQ_SEL,
                    CompareOp::DistinctFrom => 1.0 - DEFAULT_EQ_SEL,
                    _ => DEFAULT_INEQ_SEL,
                }
            }
            Expr::Function { name, args } if self.plan.string(*name) == "not" => {
                match self.plan.expr_list(*args) {
                    [operand] => 1.0 - self.selectivity(*operand),
                    _ => DEFAULT_INEQ_SEL,
                }
            }
            Expr::Constant(value) => match self.plan.value(*value) {
                Value::Boolean(true) => 1.0,
                _ => 0.0,
            },
            Expr::Column(_) => 0.5,
            _ => DEFAULT_INEQ_SEL,
        }
    }

    /// `ExplainNode`, for one node and the nodes under it.
    fn write(
        &self,
        w: &mut Writer,
        shown: &Shown,
        relationship: Option<&str>,
        plan_name: Option<&str>,
    ) {
        let options = self.options;
        let saved = w.indent;
        w.open_group("Plan", if relationship.is_some() { None } else { Some("Plan") }, true);
        if w.text() {
            if let Some(name) = plan_name {
                w.indent_text();
                w.out.push_str(name);
                w.out.push('\n');
                w.indent += 1;
            }
            if w.indent > 0 {
                w.indent_text();
                w.out.push_str("->  ");
                w.indent += 2;
            }
            w.out.push_str(shown.pname);
            w.indent += 1;
        } else {
            w.property_text("Node Type", shown.sname);
            if let Some(strategy) = shown.strategy {
                w.property_text("Strategy", strategy);
            }
            if shown.partial {
                w.property_text("Partial Mode", "Simple");
            }
            if let Some(relationship) = relationship {
                w.property_text("Parent Relationship", relationship);
            }
            if let Some(name) = plan_name {
                w.property_text("Subplan Name", name);
            }
            w.property_bool("Parallel Aware", false);
            w.property_bool("Async Capable", false);
            if let Some(rudb) = shown.rudb {
                w.property_text("rudb", rudb);
            }
        }
        if let Some(target) = &shown.target {
            scan_target(w, target);
        }
        if let Some(join) = shown.join {
            if !w.text() {
                w.property_text("Join Type", join);
            } else if join != "Inner" {
                w.out.push_str(&format!(" {join} Join"));
            } else if shown.pname != "Nested Loop" {
                w.out.push_str(" Join");
            }
        }
        if let Some(command) = shown.command {
            if w.text() {
                w.out.push(' ');
                w.out.push_str(command);
            } else {
                w.property_text("Command", command);
            }
        }
        if let (true, Some(rudb)) = (w.text(), shown.rudb) {
            w.out.push_str(&format!(" [{rudb}]"));
        }
        let cost = shown.cost;
        if options.costs {
            #[expect(clippy::cast_possible_truncation, reason = "a row width fits in 32 bits")]
            let width = cost.width as i64;
            if w.text() {
                w.out.push_str(&format!(
                    "  (cost={}..{} rows={} width={width})",
                    fixed(cost.startup, 2),
                    fixed(cost.total, 2),
                    fixed(cost.rows, 0)
                ));
            } else {
                w.property_float("Startup Cost", None, cost.startup, 2);
                w.property_float("Total Cost", None, cost.total, 2);
                w.property_float("Plan Rows", None, cost.rows, 0);
                w.property_integer("Plan Width", None, width);
            }
        }
        if self.analyze() {
            self.write_actual(w, shown.actual);
        }
        if w.text() {
            w.out.push('\n');
        } else {
            w.property_bool("Disabled", false);
        }
        if options.verbose && !shown.output.is_empty() {
            w.property_list("Output", &shown.output);
        }
        if let (Some(unique), false) = (shown.unique, w.text()) {
            w.property_bool("Inner Unique", unique);
        }
        for prop in &shown.keys {
            self.write_prop(w, prop);
        }
        if let Some(filter) = &shown.filter {
            w.property_text("Filter", filter);
        }
        let removed = shown.removed.map(|count| Prop::Removed("Rows Removed by Filter", count));
        if let (Some(removed), false) = (&removed, shown.removed_last) {
            self.write_prop(w, removed);
        }
        for prop in &shown.info {
            self.write_prop(w, prop);
        }
        if let (Some(removed), true) = (&removed, shown.removed_last) {
            self.write_prop(w, removed);
        }
        if self.analyze() && options.buffers {
            zeros(w, &BUFFER_KEYS);
        }
        if self.analyze() && options.wal {
            zeros(w, &WAL_KEYS);
        }
        if !shown.children.is_empty() {
            w.open_group("Plans", Some("Plans"), false);
            for child in &shown.children {
                self.write(w, &child.node, Some(child.relationship), child.plan_name.as_deref());
            }
            w.close_group("Plans", false);
        }
        if w.text() {
            w.indent = saved;
        }
        w.close_group("Plan", true);
    }

    /// What a node did, or that it never ran.
    fn write_actual(&self, w: &mut Writer, actual: Option<Actual>) {
        let timing = self.options.timing;
        match actual {
            Some(actual) => {
                #[expect(clippy::cast_precision_loss, reason = "a row count")]
                let rows = actual.rows as f64;
                let (startup, total) =
                    (milliseconds(actual.startup_ns), milliseconds(actual.total_ns));
                if w.text() {
                    w.out.push_str(" (actual ");
                    if timing {
                        w.out.push_str(&format!(
                            "time={}..{} ",
                            fixed(startup, 3),
                            fixed(total, 3)
                        ));
                    }
                    w.out.push_str(&format!("rows={} loops=1)", fixed(rows, 2)));
                } else {
                    if timing {
                        w.property_float("Actual Startup Time", Some("ms"), startup, 3);
                        w.property_float("Actual Total Time", Some("ms"), total, 3);
                    }
                    w.property_float("Actual Rows", None, rows, 2);
                    w.property_float("Actual Loops", None, 1.0, 0);
                }
            }
            None if w.text() => w.out.push_str(" (never executed)"),
            None => {
                if timing {
                    w.property_float("Actual Startup Time", Some("ms"), 0.0, 3);
                    w.property_float("Actual Total Time", Some("ms"), 0.0, 3);
                }
                w.property_float("Actual Rows", None, 0.0, 0);
                w.property_float("Actual Loops", None, 0.0, 0);
            }
        }
    }

    fn write_prop(&self, w: &mut Writer, prop: &Prop) {
        match prop {
            Prop::Text(label, value) => w.property_text(label, value),
            Prop::List(label, values) => w.property_list(label, values),
            Prop::Removed(label, count) => {
                if self.analyze() && (*count > 0 || !w.text()) {
                    #[expect(clippy::cast_precision_loss, reason = "a row count")]
                    w.property_float(label, None, *count as f64, 0);
                }
            }
            Prop::Sort { method, kb } => {
                if w.text() {
                    w.indent_text();
                    w.out.push_str(&format!("Sort Method: {method}  Memory: {kb}kB\n"));
                } else {
                    w.property_text("Sort Method", method);
                    w.property_uinteger("Sort Space Used", Some("kB"), *kb);
                    w.property_text("Sort Space Type", "Memory");
                }
            }
            Prop::Hash { buckets, kb } => {
                if w.text() {
                    w.indent_text();
                    w.out.push_str(&format!(
                        "Buckets: {buckets}  Batches: 1  Memory Usage: {kb}kB\n"
                    ));
                } else {
                    w.property_uinteger("Hash Buckets", None, *buckets);
                    w.property_uinteger("Original Hash Buckets", None, *buckets);
                    w.property_integer("Hash Batches", None, 1);
                    w.property_integer("Original Hash Batches", None, 1);
                    w.property_uinteger("Peak Memory Usage", Some("kB"), *kb);
                }
            }
            Prop::HashAgg { kb } => {
                if w.text() {
                    if let Some(kb) = kb {
                        w.indent_text();
                        w.out.push_str(&format!("Batches: 1  Memory Usage: {kb}kB\n"));
                    }
                } else {
                    if self.options.costs {
                        w.property_integer("Planned Partitions", None, 0);
                    }
                    if let Some(kb) = kb {
                        w.property_integer("HashAgg Batches", None, 1);
                        w.property_uinteger("Peak Memory Usage", Some("kB"), *kb);
                        w.property_integer("Disk Usage", Some("kB"), 0);
                    }
                }
            }
            Prop::Storage { kb } => {
                if w.text() {
                    w.indent_text();
                    w.out.push_str(&format!("Storage: Memory  Maximum Storage: {kb}kB\n"));
                } else {
                    w.property_text("Storage", "Memory");
                    w.property_uinteger("Maximum Storage", Some("kB"), *kb);
                }
            }
        }
    }
}

/// `ExplainScanTarget`.
fn scan_target(w: &mut Writer, target: &Target) {
    if w.text() {
        w.out.push_str(" on");
        match (&target.schema, &target.object) {
            (Some(schema), Some(object)) => {
                w.out.push_str(&format!(
                    " {}.{}",
                    quote_identifier(schema),
                    quote_identifier(object)
                ));
            }
            (None, Some(object)) => w.out.push_str(&format!(" {}", quote_identifier(object))),
            _ => {}
        }
        if target.object.as_deref() != Some(target.refname.as_str()) {
            w.out.push_str(&format!(" {}", quote_identifier(&target.refname)));
        }
    } else {
        if let (Some(tag), Some(object)) = (target.tag, &target.object) {
            w.property_text(tag, object);
        }
        if let Some(schema) = &target.schema {
            w.property_text("Schema", schema);
        }
        w.property_text("Alias", &target.refname);
    }
}

/// A sort key with the options `show_sortorder_options` writes.
fn sort_key(deparse: &Deparse<'_>, key: &SortKey) -> String {
    let mut text = deparse.expr(key.expr);
    if key.descending {
        text.push_str(" DESC");
    }
    match (key.nulls_first, key.descending) {
        (true, false) => text.push_str(" NULLS FIRST"),
        (false, true) => text.push_str(" NULLS LAST"),
        _ => {}
    }
    text
}

/// A window frame as `get_window_frame_options` writes it. `BETWEEN` is left out when the frame
/// ends at the current row, which is how the frame was most likely written.
fn frame_text(deparse: &Deparse<'_>, frame: WindowFrame) -> String {
    let bound = |bound: WindowBound| match bound {
        WindowBound::UnboundedPreceding => "UNBOUNDED PRECEDING".to_owned(),
        WindowBound::Preceding(at) => format!("{} PRECEDING", deparse.expr(at)),
        WindowBound::CurrentRow => "CURRENT ROW".to_owned(),
        WindowBound::Following(at) => format!("{} FOLLOWING", deparse.expr(at)),
        WindowBound::UnboundedFollowing => "UNBOUNDED FOLLOWING".to_owned(),
    };
    let mut text = match frame.unit {
        WindowUnit::Rows => "ROWS ",
        WindowUnit::Range => "RANGE ",
        WindowUnit::Groups => "GROUPS ",
    }
    .to_owned();
    if frame.end == WindowBound::CurrentRow {
        text.push_str(&bound(frame.start));
    } else {
        text.push_str(&format!("BETWEEN {} AND {}", bound(frame.start), bound(frame.end)));
    }
    match frame.exclude {
        WindowExclude::NoOthers => {}
        WindowExclude::CurrentRow => text.push_str(" EXCLUDE CURRENT ROW"),
        WindowExclude::Group => text.push_str(" EXCLUDE GROUP"),
        WindowExclude::Ties => text.push_str(" EXCLUDE TIES"),
    }
    text
}

/// A list of conditions as one, the way `make_ands_explicit` makes it.
fn anded(mut conditions: Vec<String>) -> String {
    match conditions.len() {
        1 => conditions.remove(0),
        _ => format!("({})", conditions.join(" AND ")),
    }
}

/// The PostgreSQL join type of a rudb join kind, and rudb's name for a kind PostgreSQL does not
/// have. A join that builds its left side is written the way PostgreSQL writes one that hashes its
/// outer side, with the two sides swapped and the type turned round.
fn join_type(kind: JoinKind, mirrored: bool) -> (&'static str, Option<&'static str>) {
    let (name, rudb) = match kind {
        JoinKind::Inner => ("Inner", None),
        JoinKind::Left => ("Left", None),
        JoinKind::Right => ("Right", None),
        JoinKind::Full => ("Full", None),
        JoinKind::Semi => ("Semi", None),
        JoinKind::Anti => ("Anti", None),
        JoinKind::Single => ("Left", Some("Single Join")),
        JoinKind::Mark => ("Semi", Some("Mark Join")),
        JoinKind::Positional => ("Inner", Some("Positional Join")),
    };
    let name = match (mirrored, name) {
        (true, "Left") => "Right",
        (true, "Right") => "Left",
        (true, "Semi") => "Right Semi",
        (true, "Anti") => "Right Anti",
        (_, name) => name,
    };
    (name, rudb)
}

/// The rows of a join from the rows of its sides and the share of pairs its conditions keep, as
/// `calc_joinrel_size_estimate` works it out.
fn join_rows(kind: JoinKind, mirrored: bool, outer: f64, inner: f64, selectivity: f64) -> f64 {
    let (kept, other) = if mirrored { (inner, outer) } else { (outer, inner) };
    let pairs = outer * inner * selectivity;
    let rows = match kind {
        JoinKind::Inner | JoinKind::Positional => pairs,
        JoinKind::Left | JoinKind::Single => pairs.max(kept),
        JoinKind::Right => pairs.max(other),
        JoinKind::Full => pairs.max(outer).max(inner),
        JoinKind::Semi | JoinKind::Mark => kept * (other * selectivity).min(1.0),
        JoinKind::Anti => kept * (1.0 - (other * selectivity).min(1.0)),
    };
    clamp(rows)
}

/// `get_variable_numdistinct` for a column nobody counted.
fn distinct(rows: f64) -> f64 {
    rows.clamp(1.0, DEFAULT_NUM_DISTINCT)
}

/// `estimate_num_groups` over columns nobody counted.
fn groups(rows: f64, keys: usize) -> f64 {
    let keys = i32::try_from(keys).unwrap_or(i32::MAX);
    clamp(DEFAULT_NUM_DISTINCT.powi(keys).min(rows))
}

/// The table indexes of the columns a node produces.
fn tables(plan: &Plan, at: NodeRef) -> HashSet<u32> {
    outputs(plan, at).into_iter().map(|(column, _)| column.table).collect()
}

/// A row estimate, with the thousand rows of a relation nobody knows anything about.
fn estimated(stat: Stat<u64>) -> f64 {
    clamp(estimated_or(stat, DEFAULT_FUNCTION_ROWS))
}

#[expect(clippy::cast_precision_loss, reason = "an estimate")]
fn estimated_or(stat: Stat<u64>, unknown: f64) -> f64 {
    match stat {
        Stat::Known { value, .. } => value as f64,
        _ => unknown,
    }
}
