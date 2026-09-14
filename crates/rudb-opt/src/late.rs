//! Late materialisation: reading the wide columns after the limit rather than before it.
//!
//! `SELECT * FROM hits ORDER BY EventTime LIMIT 10` over a hundred and five columns needs one
//! column of every row to decide which ten rows win and all hundred and five columns of the ten
//! that did. The plan the binder builds reads all hundred and five of every row and hands them
//! through the top N, which throws away everything but ten. Measured on one million ClickBench
//! rows, that query was 2.65 seconds where the pinned DuckDB binary answered it in 0.09, and the
//! whole of the difference is the hundred and four columns nobody looks at.
//!
//! This rewrite narrows the scan under the top N to the ordering columns, the filter columns and
//! the row's ordinal inside its file, and puts a [`Node::Fetch`] above the top N to read the rest
//! back for the rows that survived. The scan narrowing is not done here: inserting a projection
//! that reads only what the top N needs is enough for [`crate::columns::prune`] to do it, and this
//! pass runs it again afterwards rather than having a second copy of that rule.
//!
//! DuckDB calls this optimizer `late_materialization` and so does this, because
//! `SET disabled_optimizers = 'late_materialization'` has to turn off the pass it names.
//!
//! # What it refuses
//!
//! A top N whose input is not a projection, since the columns that are deferred have to be
//! somewhere to be deferred from.
//!
//! An ordering key that is anything but a column of the projection directly below the top N. The
//! binder gives a computed ordering such as `ORDER BY a + b` a hidden projected column, so the
//! computation can still move below the top N and be replayed for fetched rows.
//!
//! A projection whose columns are not all columns of one `read_parquet` file. The fetch reads the
//! whole row back from the file, so a column that is not in the file is a column it cannot produce.
//!
//! More than one file, because a row ordinal inside a file says which row only when there is one
//! file it could be in.
//!
//! A scan that already produces `file_row_number`, since the reader takes the last column being
//! called that as meaning it counted it and two of them would make the second a column it went
//! looking for in the file.
//!
//! A limit that is not small or a projection that is not wide, which is [`WORTH_FETCHING`] and
//! [`WORTH_DEFERRING`]. The fetch opens a row group and reads a dictionary page per column per row
//! group it touches, so at some number of rows it costs what the scan would have cost.
//!
//! # Rebuilding rather than writing in place
//!
//! Three nodes go where two were, and a node has to come after its children in the arena, so there
//! is no slot to write the fetch into. The walk rebuilds the path from the root down to whatever
//! changed, which is what `filter` does for the same reason, and leaves the nodes it replaced
//! behind unreachable.

use rudb_common::{Field, LogicalType, Result, Value};
use rudb_functions::FILE_ROW_NUMBER;
use std::collections::HashMap;

use rudb_plan::{ColumnBinding, Expr, Node, NodeRef, Plan, Slice, SortKey};

use crate::pass::{Context, Pass};
use crate::walk;

/// The most rows a fetch is worth doing for.
///
/// A fetch reads a page per column per row group it touches, and the dictionary page of a
/// ClickBench column chunk is most of the chunk, so the floor is the row groups the winners land
/// in rather than the winners themselves. Two rows out of a million cost 31 MB of the 220 MB the
/// file holds, which is a fifth of the scan and not a thousandth of it. At a few thousand rows
/// spread over a file every row group is opened and there is nothing left to save. Every
/// ClickBench query that ends in a limit ends in `LIMIT 10`.
pub const WORTH_FETCHING: u64 = 1024;

/// How many more columns than the ordering needs make the deferral worth the second read.
///
/// The fetch reads the ordering column again along with everything else, so a projection that is
/// the ordering columns and a couple more is a rewrite that saves two columns and costs one. A
/// hundred and five columns against one is the case this exists for.
pub const WORTH_DEFERRING: usize = 8;

/// Defers the columns a top N does not order by until after it has chosen its rows.
#[derive(Debug, Clone, Copy)]
pub struct LateMaterialization;

impl Pass for LateMaterialization {
    fn name(&self) -> &'static str {
        "late_materialization"
    }

    fn run(&self, plan: &mut Plan, _context: &Context) -> Result<()> {
        defer(plan);
        Ok(())
    }
}

/// Rewrites every top N in `plan` that is worth it into a narrow one with a fetch above it.
///
/// Rewrites by rebuilding, so the root moves when anything changed. A plan this has already run
/// over is left alone the second time, because the top N it produces sits over a projection whose
/// columns are the ordering ones and there is nothing left to defer.
pub fn defer(plan: &mut Plan) {
    let mut deferred = false;
    let root = rewrite(plan, plan.root(), &mut deferred);
    if !deferred {
        return;
    }
    plan.set_root(root);
    // The narrowing this opens up is column pruning's, and running it again here is what turns the
    // projection this leaves under the top N into a scan of two columns rather than a scan of a
    // hundred and five with a projection over it.
    crate::columns::prune(plan);
}

/// Rewrites the subtree at `at`, handing back whatever is at its top afterwards.
fn rewrite(plan: &mut Plan, at: NodeRef, deferred: &mut bool) -> NodeRef {
    let children = plan.node(at).children();
    let rebuilt: Vec<NodeRef> =
        children.into_iter().flatten().map(|child| rewrite(plan, child, deferred)).collect();
    let mut here = at;
    let moved = children.into_iter().flatten().zip(&rebuilt).any(|(was, &now)| was != now);
    if moved {
        let mut node = plan.node(at).clone();
        replace_children(&mut node, &rebuilt);
        here = plan.add_node(node);
    }
    match fetch(plan, here) {
        Some(above) => {
            *deferred = true;
            above
        }
        None => here,
    }
}

/// Points a node at a new set of children, in the order [`Node::children`] hands them back.
fn replace_children(node: &mut Node, children: &[NodeRef]) {
    match node {
        Node::Filter { input, .. }
        | Node::Project { input, .. }
        | Node::Aggregate { input, .. }
        | Node::Sort { input, .. }
        | Node::Limit { input, .. }
        | Node::TopN { input, .. }
        | Node::Fetch { input, .. }
        | Node::Distinct { input, .. } => *input = children[0],
        Node::Join { left, right, .. }
        | Node::CrossProduct { left, right }
        | Node::SetOp { left, right, .. } => {
            *left = children[0];
            *right = children[1];
        }
        Node::Get { .. } | Node::Dummy | Node::Values { .. } | Node::TableFunction { .. } => {}
    }
}

/// The rewritten top of `at` when it is a top N this applies to, and nothing when it is not.
fn fetch(plan: &mut Plan, at: NodeRef) -> Option<NodeRef> {
    let Node::TopN { input, keys, count, offset } = *plan.node(at) else { return None };
    if count.saturating_add(offset) > WORTH_FETCHING {
        return None;
    }
    let Node::Project { input: under, index, exprs, names } = *plan.node(input) else {
        return None;
    };
    let held: Vec<_> = plan.expr_list(exprs).to_vec();
    let labels: Vec<_> = plan.name_list(names).to_vec();
    let ordering: Vec<SortKey> = plan.sort_key_list(keys).to_vec();
    if held.len() < ordering.len() + WORTH_DEFERRING {
        return None;
    }

    // Every key has to be a bare column of this projection, so that the narrowed projection can
    // produce the key columns and the new keys can be written against it without copying a tree.
    let mut wanted = Vec::with_capacity(ordering.len());
    for key in &ordering {
        match *plan.expr(key.expr) {
            Expr::Column(binding) if binding.table == index => {
                let at = binding.column as usize;
                wanted.push((*held.get(at)?, *labels.get(at)?));
            }
            _ => return None,
        }
    }

    let chain = chain(plan, under)?;
    let scan = *chain.last()?;
    let columns = file_columns(plan, &chain, &held);
    let deferred_projects = if columns.is_none() {
        let scan_index = file_index(plan, scan)?;
        let projects = projects(plan, &chain, index, &held, &labels);
        replayable(plan, scan_index, &projects).then_some((scan_index, projects))
    } else {
        None
    };
    if columns.is_none() && deferred_projects.is_none() {
        return None;
    }
    let columns = match columns {
        Some(columns) => columns,
        None => {
            (0..file_width(plan, scan)?).map(|at| u32::try_from(at).ok()).collect::<Option<_>>()?
        }
    };

    // Bottom up, because each level's new column refers to the one below it. What comes back is the
    // ordinal as the projection under the top N will produce it.
    let mut carried = number(plan, scan)?;
    for &node in chain.iter().rev().skip(1) {
        carried = carry(plan, node, carried);
    }
    let row = plan.add_expr(Expr::Column(carried), LogicalType::BigInt);

    let narrow = narrow(plan, under, &wanted, row);
    let above = top(plan, narrow, &ordering, count, offset);
    let deferred = fields(plan, scan, &columns);
    let args = match *plan.node(scan) {
        Node::TableFunction { args, .. } => args,
        _ => return None,
    };
    let ordinal = plan.add_expr(
        Expr::Column(ColumnBinding::new(narrow_index(plan, narrow), wanted.len() as u32)),
        LogicalType::BigInt,
    );
    let fetched_index = if deferred_projects.is_some() { fresh(plan) } else { index };
    let fetched = plan.add_node(Node::Fetch {
        input: above,
        index: fetched_index,
        args,
        columns: deferred,
        row: ordinal,
    });
    match deferred_projects {
        Some((scan_index, projects)) => {
            Some(replay(plan, fetched, fetched_index, scan_index, projects))
        }
        None => Some(fetched),
    }
}

/// The table index of the raw file row.
fn file_index(plan: &Plan, scan: NodeRef) -> Option<u32> {
    match *plan.node(scan) {
        Node::TableFunction { index, .. } => Some(index),
        _ => None,
    }
}

/// The width of the file row a scan produces before an ordinal is appended to it.
fn file_width(plan: &Plan, scan: NodeRef) -> Option<usize> {
    match *plan.node(scan) {
        Node::TableFunction { columns, .. } => Some(
            plan.field_list(columns).iter().filter(|field| field.name != FILE_ROW_NUMBER).count(),
        ),
        _ => None,
    }
}

/// The projections that turn a raw file row into the row the top N used to produce.
fn projects(
    plan: &Plan,
    chain: &[NodeRef],
    outer_index: u32,
    outer_exprs: &[u32],
    outer_names: &[u32],
) -> Vec<(u32, Vec<u32>, Vec<u32>)> {
    let mut found = Vec::new();
    for &node in chain.iter().rev() {
        if let Node::Project { index, exprs, names, .. } = *plan.node(node) {
            found.push((index, plan.expr_list(exprs).to_vec(), plan.name_list(names).to_vec()));
        }
    }
    found.push((outer_index, outer_exprs.to_vec(), outer_names.to_vec()));
    found
}

/// Whether every computed projection can be rebuilt from the file row below it.
fn replayable(plan: &Plan, scan_index: u32, projects: &[(u32, Vec<u32>, Vec<u32>)]) -> bool {
    let mut tables = std::collections::HashSet::from([scan_index]);
    for (index, exprs, _) in projects {
        for &expr in exprs {
            let mut missing = false;
            walk::columns(plan, expr, &mut |binding| missing |= !tables.contains(&binding.table));
            if missing {
                return false;
            }
        }
        tables.insert(*index);
    }
    true
}

/// Rebuilds deferred computed projections over the raw rows a fetch returned.
fn replay(
    plan: &mut Plan,
    mut input: NodeRef,
    fetched_index: u32,
    scan_index: u32,
    projects: Vec<(u32, Vec<u32>, Vec<u32>)>,
) -> NodeRef {
    let mut tables = HashMap::from([(scan_index, fetched_index)]);
    let count = projects.len();
    for (at, (old_index, exprs, names)) in projects.into_iter().enumerate() {
        let rewritten: Vec<u32> =
            exprs.into_iter().map(|expr| rebase(plan, expr, &tables)).collect();
        let exprs = plan.add_expr_list(&rewritten);
        let names = plan.add_name_list(&names);
        let index = if at + 1 == count { old_index } else { fresh(plan) };
        input = plan.add_node(Node::Project { input, index, exprs, names });
        tables.insert(old_index, index);
    }
    input
}

/// Copies one expression while changing the table indexes of its column references.
fn rebase(plan: &mut Plan, expr: u32, tables: &HashMap<u32, u32>) -> u32 {
    if let Expr::Column(binding) = *plan.expr(expr) {
        let table = tables.get(&binding.table).copied().unwrap_or(binding.table);
        return plan.add_expr(
            Expr::Column(ColumnBinding::new(table, binding.column)),
            plan.expr_type(expr).clone(),
        );
    }
    walk::rebuild(plan, expr, &mut |plan, child| rebase(plan, child, tables))
}

/// The table index of the projection this pass just built.
fn narrow_index(plan: &Plan, node: NodeRef) -> u32 {
    plan.node(node).table_index().unwrap_or(0)
}

/// The projection that goes under the top N: the ordering columns and then the ordinal.
fn narrow(plan: &mut Plan, input: NodeRef, wanted: &[(u32, u32)], row: u32) -> NodeRef {
    let index = fresh(plan);
    let mut exprs: Vec<u32> = wanted.iter().map(|&(expr, _)| expr).collect();
    let mut names: Vec<u32> = wanted.iter().map(|&(_, name)| name).collect();
    exprs.push(row);
    names.push(plan.intern(FILE_ROW_NUMBER));
    let exprs = plan.add_expr_list(&exprs);
    let names = plan.add_name_list(&names);
    plan.add_node(Node::Project { input, index, exprs, names })
}

/// The top N over the narrowed projection, ordering by the columns it now produces.
fn top(plan: &mut Plan, input: NodeRef, ordering: &[SortKey], count: u64, offset: u64) -> NodeRef {
    let index = narrow_index(plan, input);
    let mut keys = Vec::with_capacity(ordering.len());
    for (at, key) in ordering.iter().enumerate() {
        let column = u32::try_from(at).unwrap_or(u32::MAX);
        let ty = plan.expr_type(key.expr).clone();
        let expr = plan.add_expr(Expr::Column(ColumnBinding::new(index, column)), ty);
        keys.push(SortKey { expr, descending: key.descending, nulls_first: key.nulls_first });
    }
    let keys = plan.add_sort_keys(&keys);
    plan.add_node(Node::TopN { input, keys, count, offset })
}

/// The fields the fetch produces, which are the file's own for the columns it was asked for.
fn fields(plan: &mut Plan, scan: NodeRef, columns: &[u32]) -> Slice {
    let held = match *plan.node(scan) {
        Node::TableFunction { columns, .. } => plan.field_list(columns).to_vec(),
        _ => Vec::new(),
    };
    let wanted: Vec<Field> =
        columns.iter().filter_map(|&at| held.get(at as usize).cloned()).collect();
    plan.add_fields(&wanted)
}

/// The straight run of one input operators from `at` down to a table function, when that is what
/// is there.
///
/// A join, a set operation, an aggregate or a base table stops it, because none of those is a file
/// whose rows have an ordinal.
fn chain(plan: &Plan, at: NodeRef) -> Option<Vec<NodeRef>> {
    let mut found = vec![at];
    let mut node = at;
    loop {
        match *plan.node(node) {
            Node::TableFunction { .. } => return Some(found),
            Node::Project { input, .. }
            | Node::Filter { input, .. }
            | Node::Sort { input, .. }
            | Node::Limit { input, .. }
            | Node::TopN { input, .. }
            | Node::Distinct { input, .. } => {
                node = input;
                found.push(node);
            }
            _ => return None,
        }
    }
}

/// Which file column each of `exprs` is, when every one of them is one.
///
/// `exprs` are the projection's expressions, so they are written against the first node of the
/// chain rather than against the projection itself. Each one is followed down the chain a
/// projection at a time until it lands on the table function, and anything that is not a bare
/// column on the way gives up on the whole rewrite.
fn file_columns(plan: &Plan, chain: &[NodeRef], exprs: &[u32]) -> Option<Vec<u32>> {
    let mut carried = Vec::with_capacity(exprs.len());
    for &expr in exprs {
        match *plan.expr(expr) {
            Expr::Column(binding) => carried.push(binding),
            _ => return None,
        }
    }
    for &node in chain {
        match *plan.node(node) {
            Node::Project { index, exprs, .. } => {
                let held = plan.expr_list(exprs);
                let mut next = Vec::with_capacity(carried.len());
                for binding in &carried {
                    if binding.table != index {
                        return None;
                    }
                    match *plan.expr(*held.get(binding.column as usize)?) {
                        Expr::Column(below) => next.push(below),
                        _ => return None,
                    }
                }
                carried = next;
            }
            Node::TableFunction { index, .. } => {
                if carried.iter().any(|binding| binding.table != index) {
                    return None;
                }
                return Some(carried.into_iter().map(|binding| binding.column).collect());
            }
            _ => {}
        }
    }
    None
}

/// Turns `file_row_number` on for a scan and hands back the column it now produces.
///
/// Nothing when the scan is not a single file `read_parquet`, or when it already produces a column
/// of that name, since the reader reads the last column being called that as the one it counted.
fn number(plan: &mut Plan, scan: NodeRef) -> Option<ColumnBinding> {
    let Node::TableFunction { index, function, args, options, settings, columns } =
        *plan.node(scan)
    else {
        return None;
    };
    if plan.string(function) != "read_parquet" || plan.expr_list(args).len() != 1 {
        return None;
    }
    let mut fields = plan.field_list(columns).to_vec();
    if fields.iter().any(|field| field.name == FILE_ROW_NUMBER) {
        return None;
    }
    let at = u32::try_from(fields.len()).ok()?;
    fields.push(Field::required(FILE_ROW_NUMBER.to_string(), LogicalType::BigInt));
    let widened = plan.add_fields(&fields);

    let mut names = plan.name_list(options).to_vec();
    let mut values = plan.expr_list(settings).to_vec();
    names.push(plan.intern(FILE_ROW_NUMBER));
    values.push(plan.add_constant(Value::Boolean(true)));
    let named = plan.add_name_list(&names);
    let given = plan.add_expr_list(&values);

    match plan.node_mut(scan) {
        Node::TableFunction { columns, options, settings, .. } => {
            *columns = widened;
            *options = named;
            *settings = given;
        }
        _ => return None,
    }
    Some(ColumnBinding::new(index, at))
}

/// Carries the ordinal through one node of the chain, appending a column where the node has any.
fn carry(plan: &mut Plan, node: NodeRef, below: ColumnBinding) -> ColumnBinding {
    let Node::Project { index, exprs, names, .. } = *plan.node(node) else { return below };
    let mut held = plan.expr_list(exprs).to_vec();
    let mut labels = plan.name_list(names).to_vec();
    let at = u32::try_from(held.len()).unwrap_or(u32::MAX);
    held.push(plan.add_expr(Expr::Column(below), LogicalType::BigInt));
    labels.push(plan.intern(FILE_ROW_NUMBER));
    let widened = plan.add_expr_list(&held);
    let renamed = plan.add_name_list(&labels);
    match plan.node_mut(node) {
        Node::Project { exprs, names, .. } => {
            *exprs = widened;
            *names = renamed;
        }
        _ => return below,
    }
    ColumnBinding::new(index, at)
}

/// A table index no node in the plan is using.
fn fresh(plan: &Plan) -> u32 {
    let mut next = 0;
    for at in 0..plan.node_count() {
        let node = plan.node(u32::try_from(at).unwrap_or(u32::MAX));
        if let Some(index) = node.table_index() {
            next = next.max(index + 1);
        }
    }
    next
}

#[cfg(test)]
mod tests {
    use rudb_plan::Plan;

    use super::defer;

    /// A wide `read_parquet` with a top N over it, in the shape a view expansion leaves.
    ///
    /// `columns` names the file's columns and `keys` is what the top N orders by, written as the
    /// sort key text a dump uses.
    fn wide(columns: &[&str], keys: &str, extra: &str) -> String {
        let schema: Vec<String> = columns.iter().map(|name| format!("{name}::INTEGER")).collect();
        let project: Vec<String> = columns
            .iter()
            .enumerate()
            .map(|(at, name)| format!("#1.{at}::INTEGER AS {name}"))
            .collect();
        let above: Vec<String> = columns
            .iter()
            .enumerate()
            .map(|(at, name)| format!("#3.{at}::INTEGER AS {name}"))
            .collect();
        format!(
            "Project #9 [{}]\n  TopN 10 offset 0 [{keys}]\n    Project #3 [{}]\n{extra}      \
             TableFunction read_parquet args=['hits.parquet'::VARCHAR] #1 [{}]\n",
            above.join(", "),
            project.join(", "),
            schema.join(", ")
        )
    }

    /// What the plan a text prints looks like once the pass has run over it.
    fn deferred(text: &str) -> String {
        let mut plan =
            Plan::parse(text).unwrap_or_else(|error| panic!("{text} did not parse: {error}"));
        defer(&mut plan);
        plan.validate().unwrap_or_else(|error| panic!("{text} did not stay valid: {error}"));
        plan.to_string()
    }

    const TEN: [&str; 10] = ["a", "b", "c", "d", "e", "f", "g", "h", "i", "j"];

    #[test]
    fn a_top_n_over_a_wide_scan_keeps_the_ordering_column_and_fetches_the_rest() {
        let out = deferred(&wide(&TEN, "#3.1::INTEGER ASC NULLS LAST", ""));
        assert!(out.contains("Fetch args=['hits.parquet'::VARCHAR]"), "{out}");
        assert!(out.contains("file_row_number=TRUE"), "{out}");
        // The scan is down to the ordering column and the ordinal, which is what the whole rewrite
        // is for. Nine columns of ten are no longer read to answer a question about one.
        assert!(out.contains("#1 [b::INTEGER, file_row_number::BIGINT]"), "{out}");
    }

    #[test]
    fn the_answer_still_has_the_columns_the_query_asked_for() {
        let out = deferred(&wide(&TEN, "#3.0::INTEGER ASC NULLS LAST", ""));
        let fetched = out.lines().find(|line| line.contains("Fetch")).unwrap_or_default();
        for name in TEN {
            assert!(fetched.contains(&format!("{name}::INTEGER")), "{name} missing from {out}");
        }
    }

    #[test]
    fn a_filter_between_the_scan_and_the_top_n_comes_along() {
        let text = wide(&TEN, "#3.0::INTEGER ASC NULLS LAST", "").replace(
            "      TableFunction",
            "      Filter (#1.9::INTEGER = 5::INTEGER)::BOOLEAN\n        TableFunction",
        );
        let out = deferred(&text);
        assert!(out.contains("Fetch args="), "{out}");
        // The filter's column is read under the top N as well as the ordering one, because a row
        // that the filter drops is a row the top N never sees.
        assert!(out.contains("#1 [a::INTEGER, j::INTEGER, file_row_number::BIGINT]"), "{out}");
    }

    #[test]
    fn computed_file_columns_are_replayed_after_the_fetch() {
        let text = wide(&TEN, "#3.1::BIGINT ASC NULLS LAST", "")
            .replace("#1.1::INTEGER AS b", "CAST(#1.1::INTEGER)::BIGINT AS b");
        let out = deferred(&text);
        assert!(out.contains("Fetch args="), "{out}");
        assert!(out.contains("CAST(#"), "{out}");
        assert!(out.contains("[b::INTEGER, file_row_number::BIGINT]"), "{out}");
        assert!(out.lines().next().is_some_and(|line| line.starts_with("Project #9")), "{out}");
    }

    #[test]
    fn a_scan_that_is_not_much_wider_than_the_ordering_is_left_alone() {
        let text = wide(&["a", "b", "c"], "#3.0::INTEGER ASC NULLS LAST", "");
        assert!(!deferred(&text).contains("Fetch"), "{text}");
    }

    #[test]
    fn a_limit_too_large_to_be_worth_fetching_for_is_left_alone() {
        let text = wide(&TEN, "#3.0::INTEGER ASC NULLS LAST", "").replace("TopN 10", "TopN 100000");
        assert!(!deferred(&text).contains("Fetch"), "{text}");
    }

    #[test]
    fn an_ordering_that_is_not_a_bare_column_is_left_alone() {
        let text = wide(&TEN, "CAST(#3.0::INTEGER)::BIGINT ASC NULLS LAST", "");
        assert!(!deferred(&text).contains("Fetch"), "{text}");
    }

    #[test]
    fn a_top_n_over_a_base_table_is_left_alone_because_a_table_has_no_ordinals() {
        let text = wide(&TEN, "#3.0::INTEGER ASC NULLS LAST", "").replace(
            "TableFunction read_parquet args=['hits.parquet'::VARCHAR] #1",
            "Get memory.main.t AS t #1",
        );
        assert!(!deferred(&text).contains("Fetch"), "{text}");
    }

    #[test]
    fn running_it_twice_is_running_it_once() {
        let once = deferred(&wide(&TEN, "#3.0::INTEGER ASC NULLS LAST", ""));
        assert_eq!(deferred(&once), once);
    }
}
