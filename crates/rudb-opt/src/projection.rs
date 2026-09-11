//! Projection pushdown: a scan reads the columns the query uses and no others.
//!
//! `spec/09-optimizer.md` section 9.2 calls this the difference between 20 GB and 200 MB on
//! ClickBench, and on the file itself it is more than that. The first partition of `hits` is 105
//! columns and 122 MB of column data, of which the three columns query 1 reads are 8.5 MB. Before
//! this pass, `SELECT count(*)` over that file read every byte of it, because the binder projects
//! every column of a table and nothing afterwards narrowed the list.
//!
//! # What it does
//!
//! A scan's output is a list of columns and everything above it refers to those columns by
//! position. So the pass collects every column reference in the plan, drops the columns of each
//! scan that nothing refers to, and moves the references that come after a dropped column along to
//! where their column now is. That second half is the part that has to be right: a scan that reads
//! fewer columns and a plan that still asks for position 7 of it is a wrong answer rather than a
//! failure.
//!
//! # What it does not do
//!
//! It narrows [`Node::Get`] and [`Node::TableFunction`] and leaves every other node alone. A
//! `VALUES` list is already in the plan, so dropping a column of one saves reading nothing, and a
//! projection above another projection is expression level work that belongs to the pass that
//! eliminates dead expressions rather than to this one.
//!
//! A scan nothing refers to at all comes out with no columns, which is what `SELECT count(*)` asks
//! for, and the readers below produce a chunk that is a row count and nothing else. That case is
//! why the pass is worth landing before the ones that are worth more: it is the cheapest query in
//! ClickBench and it was reading the whole file.

use std::collections::{BTreeSet, HashMap};

use rudb_common::{Error, Result};
use rudb_plan::{ColumnBinding, Expr, Node, Plan};

use crate::walk;

/// Narrows every scan in `plan` to the columns the rest of the plan refers to.
///
/// # Errors
///
/// If the plan does not hold together, which here means an expression that was walked to as a
/// column reference and is not one.
pub(crate) fn push_down(plan: &mut Plan) -> Result<()> {
    let nodes = walk::nodes(plan);
    let mut roots = Vec::new();
    for &node in &nodes {
        roots.extend(walk::roots(plan, node));
    }
    let reachable = walk::exprs(plan, &roots);

    // Which columns of which scan are used. A table index that is not in here is a scan nothing
    // reads, and that is not the same as a scan that is not in the plan.
    let mut used: HashMap<u32, BTreeSet<u32>> = HashMap::new();
    for &reference in &reachable {
        if let Expr::Column(binding) = *plan.expr(reference) {
            used.entry(binding.table).or_default().insert(binding.column);
        }
    }

    // Where each surviving column moves to. Only the scans that lose a column are in here, so a
    // plan that uses everything it reads produces an empty map and no rewriting at all.
    let mut moved: HashMap<ColumnBinding, u32> = HashMap::new();
    for &node in &nodes {
        let (index, columns) = match *plan.node(node) {
            Node::Get { index, columns, .. } | Node::TableFunction { index, columns, .. } => {
                (index, columns)
            }
            _ => continue,
        };
        let held = plan.field_list(columns).len();
        let empty = BTreeSet::new();
        let wanted = used.get(&index).unwrap_or(&empty);
        if wanted.len() == held {
            continue;
        }

        let fields = plan.field_list(columns).to_vec();
        let mut kept = Vec::with_capacity(wanted.len());
        for &at in wanted {
            let field = fields.get(at as usize).ok_or_else(|| {
                Error::internal(format!(
                    "column {at} of a scan that produces {} columns",
                    fields.len()
                ))
            })?;
            kept.push(field.clone());
        }
        for (to, &from) in wanted.iter().enumerate() {
            let to = u32::try_from(to).expect("a scan of four billion columns is not a scan");
            if to != from {
                moved.insert(ColumnBinding::new(index, from), to);
            }
        }
        let narrowed = plan.add_fields(&kept);
        let node_now = match plan.node(node).clone() {
            Node::Get { catalog, schema, table, alias, index, .. } => {
                Node::Get { catalog, schema, table, alias, index, columns: narrowed }
            }
            Node::TableFunction { index, function, args, .. } => {
                Node::TableFunction { index, function, args, columns: narrowed }
            }
            other => other,
        };
        plan.set_node(node, node_now);
    }

    for &reference in &reachable {
        let Expr::Column(binding) = *plan.expr(reference) else {
            continue;
        };
        if let Some(&column) = moved.get(&binding) {
            plan.rebind_column(reference, ColumnBinding::new(binding.table, column))?;
        }
    }
    Ok(())
}
