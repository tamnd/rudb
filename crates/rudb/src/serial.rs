//! Serializable transactions, the ones begun `ISOLATION LEVEL SERIALIZABLE`.
//!
//! Snapshot isolation lets two transactions that each read what the other one writes both commit.
//! That is write skew: two doctors on call each see the other one still on, and both go off. A
//! serializable transaction notes what every query it runs reads, which is the table and the filters
//! the plan puts straight over the scan, and its commit checks those reads against the rows
//! committed since its snapshot. A row somebody else changed that a read picks, as the row was in
//! the snapshot or as it is now, is a row the transaction would not read the same way any more, and
//! the commit fails rather than leave a result that no order of the two could give. This is
//! precision locking done at the commit, the way Neumann, Mühlbauer and Kemper validate
//! serializable MVCC in HyPer. The reads are predicates, so a row added since that a filter picks,
//! a phantom, fails the commit too.
//!
//! What a read notes is never less than the query read and is often more. A filter is every row it
//! picks however few of them the query went on to use, a join or a `LIMIT` above it cuts nothing,
//! and a scan with no filter straight over it reads the whole table. A transaction that is not
//! serializable notes nothing and pays nothing.

use std::collections::BTreeSet;
use std::sync::{Arc, Mutex, PoisonError};

use rudb_catalog::{Catalog, QualifiedName, Table};
use rudb_common::{Error, Field, Result, Value};
use rudb_exec::Schema;
use rudb_plan::{ExprRef, Node, NodeRef, Plan};
use rudb_vector::{Chunk, Selection};

use crate::txn::Marks;

/// What a connection's serializable transaction has read so far, and nothing when the connection
/// has no such transaction open.
#[derive(Debug, Default)]
pub(crate) struct Reads(Mutex<Option<Vec<Read>>>);

impl Reads {
    /// Starts noting for a transaction just begun, or stops for one that is not serializable.
    pub(crate) fn begin(&self, serializable: bool) {
        *self.held() = serializable.then(Vec::new);
    }

    /// What the transaction read, which stops the noting.
    pub(crate) fn take(&self) -> Option<Vec<Read>> {
        self.held().take()
    }

    /// Whether the open transaction is a serializable one.
    pub(crate) fn noting(&self) -> bool {
        self.held().is_some()
    }

    /// Notes what the plan reads, run against `catalog`.
    pub(crate) fn note(&self, plan: &Plan, catalog: &Catalog) {
        if let Some(reads) = self.held().as_mut() {
            reads.extend(reading(plan, catalog, false));
        }
    }

    /// Notes what the source of an `UPDATE` or a `DELETE` reads, which is a project over the table
    /// whose last column is the flag that says which rows the statement changes. Its rows are the
    /// ones the flag picks rather than every row the source passes on.
    pub(crate) fn note_change(&self, plan: &Plan, catalog: &Catalog) {
        if let Some(reads) = self.held().as_mut() {
            reads.extend(reading(plan, catalog, true));
        }
    }

    /// Notes a read of every row of the table `oid`, for what reads a table without a plan.
    pub(crate) fn note_table(&self, oid: i64) {
        if let Some(reads) = self.held().as_mut() {
            reads.push(Read { oid, picked: None });
        }
    }

    fn held(&self) -> std::sync::MutexGuard<'_, Option<Vec<Read>>> {
        self.0.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

/// What one query of a serializable transaction read of one table.
#[derive(Debug)]
pub(crate) struct Read {
    pub(crate) oid: i64,
    /// The rows the read picked, or `None` for every row.
    picked: Option<Picked>,
}

/// The rows of a table a read picked: the columns the scan read, then the filters and projects the
/// plan put over the scan, from the scan up to the last filter.
#[derive(Debug)]
struct Picked {
    plan: Arc<Plan>,
    /// The table index the scan's columns bind against.
    index: u32,
    /// The columns the scan read.
    fields: Vec<Field>,
    steps: Vec<Step>,
}

#[derive(Debug, Clone)]
enum Step {
    /// Keeps the rows the predicate is true for.
    Keep(ExprRef),
    /// Works out new columns, bound against `index`.
    Make { index: u32, exprs: Vec<ExprRef> },
}

/// The reads of `plan`, one for each table it scans.
fn reading(plan: &Plan, catalog: &Catalog, change: bool) -> Vec<Read> {
    let mut scans = Vec::new();
    match *plan.node(plan.root()) {
        Node::Project { input, exprs, .. } if change => {
            let above = plan.expr_list(exprs).last().map(|&flag| Step::Keep(flag));
            scans_of(plan, input, above.into_iter().collect(), &mut scans);
        }
        _ => scans_of(plan, plan.root(), Vec::new(), &mut scans),
    }
    let mut shared: Option<Arc<Plan>> = None;
    let mut reads = Vec::new();
    for (scan, mut above) in scans {
        let (catalog_name, schema, table, index, columns) = match *plan.node(scan) {
            Node::Get { catalog, schema, table, index, columns, .. } => {
                (catalog, schema, table, Some(index), columns)
            }
            Node::TableFetch { catalog, schema, table, columns, .. } => {
                (catalog, schema, table, None, columns)
            }
            _ => continue,
        };
        let name =
            QualifiedName::new(plan.string(catalog_name), plan.string(schema), plan.string(table));
        // A scan of something that is not a stored table, which a commit cannot change.
        let Ok(table) = catalog.table(&name) else { continue };
        let oid = table.oid();
        above.reverse();
        // What comes after the last filter picks no fewer rows than it does.
        let kept = above.iter().rposition(|step| matches!(step, Step::Keep(_)));
        let picked = match (index, kept) {
            (Some(index), Some(last)) => {
                above.truncate(last + 1);
                let plan = Arc::clone(shared.get_or_insert_with(|| Arc::new(plan.clone())));
                let fields = plan.field_list(columns).to_vec();
                Some(Picked { plan, index, fields, steps: above })
            }
            _ => None,
        };
        reads.push(Read { oid, picked });
    }
    reads
}

/// Finds the scans under `node`, each with the filters and projects straight over it, outermost
/// first, in `above`.
fn scans_of(
    plan: &Plan,
    node: NodeRef,
    mut above: Vec<Step>,
    scans: &mut Vec<(NodeRef, Vec<Step>)>,
) {
    match *plan.node(node) {
        Node::Get { .. } => scans.push((node, above)),
        Node::Filter { input, predicate } => {
            above.push(Step::Keep(predicate));
            scans_of(plan, input, above, scans);
        }
        Node::Project { input, index, exprs, .. } => {
            above.push(Step::Make { index, exprs: plan.expr_list(exprs).to_vec() });
            scans_of(plan, input, above, scans);
        }
        // Rows read back by number are rows of the whole table as far as this can tell.
        Node::TableFetch { input, .. } => {
            scans.push((node, Vec::new()));
            scans_of(plan, input, Vec::new(), scans);
        }
        Node::Consistent { reducer, .. } => {
            for leaf in &plan.reducer(reducer).leaves {
                scans_of(plan, leaf.input, Vec::new(), scans);
            }
        }
        ref other => {
            for child in other.children().into_iter().flatten() {
                scans_of(plan, child, Vec::new(), scans);
            }
        }
    }
}

/// Fails the commit of a serializable transaction when one of its reads picks a row committed since
/// its snapshot. `base` is the snapshot's catalog and `now` the committed one, and `marks` gives
/// the rows each commit since the snapshot updated or deleted in a table, which the registry kept
/// because this transaction was open.
///
/// # Errors
///
/// The serialization failure, or an error reading the rows.
pub(crate) fn check(
    reads: &[Read],
    base: &Catalog,
    now: &Catalog,
    marks: impl Fn(i64) -> Vec<Marks>,
) -> Result<()> {
    for read in reads {
        // A table the transaction made itself, which nobody else could change.
        let Some(before) = by_oid(base, read.oid) else { continue };
        let met = match by_oid(now, read.oid) {
            Some(after) if after.revision() == before.revision() => false,
            Some(after) => met(read, before, after, &marks(read.oid))?,
            None => true,
        };
        if met {
            return Err(failure());
        }
    }
    Ok(())
}

/// The error a serializable commit fails with, in PostgreSQL's words for it.
pub(crate) fn failure() -> Error {
    Error::transaction(
        "Failed to commit: could not serialize access due to read/write dependencies among \
         transactions",
    )
}

/// Whether the rows of the table changed between `before` and `after` include one `read` picks.
///
/// When every change since kept the rows where they were, the rows to look at are the ones the
/// marks name and the ones added on the end, each as it was and as it is. Otherwise the rows moved
/// and cannot be matched up one for one, and the rows the read picks from each table are compared
/// whole.
fn met(read: &Read, before: &Table, after: &Table, marks: &[Marks]) -> Result<bool> {
    let frame = before.frame();
    let (old, new) = (before.rows().len() as u64, after.rows().len() as u64);
    let placed = after.frame() == frame
        && new >= old
        && marks.iter().all(|marks| !marks.all && marks.frame == frame);
    if !placed {
        return Ok(match (rows_picked(read, before)?, rows_picked(read, after)?) {
            (Some(was), Some(is)) => was != is,
            _ => true,
        });
    }
    let changed: BTreeSet<u64> =
        marks.iter().flat_map(|marks| marks.rows.iter().copied()).collect();
    let was: Vec<u64> = changed.range(..old).copied().collect();
    let is: Vec<u64> = changed
        .range(..new)
        .copied()
        .chain(old..new)
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect();
    Ok(picks_any(read, before, &was)? || picks_any(read, after, &is)?)
}

/// Whether `read` picks any of the rows of `table` numbered `rows`, which rise.
fn picks_any(read: &Read, table: &Table, rows: &[u64]) -> Result<bool> {
    if rows.is_empty() {
        return Ok(false);
    }
    let Some(picked) = &read.picked else { return Ok(true) };
    let Some(columns) = columns(picked, table) else { return Ok(true) };
    let types: Vec<_> = picked.fields.iter().map(|field| field.ty.clone()).collect();
    let chunk = table.rows().rows_at(&types, &columns, rows)?;
    // A filter that cannot be worked out over the rows is taken to pick them.
    Ok(picked.pick(chunk).is_none_or(|kept| !kept.is_empty()))
}

/// The rows of `table` that `read` picks, as text and sorted, for comparing two tables whose rows
/// are not numbered alike. `None` stands for rows the filter could not be worked out over.
fn rows_picked(read: &Read, table: &Table) -> Result<Option<Vec<String>>> {
    let all: Vec<usize>;
    let (columns, picked) = match &read.picked {
        Some(picked) => match columns(picked, table) {
            Some(columns) => (columns, Some(picked)),
            None => return Ok(None),
        },
        None => {
            all = (0..table.columns().len()).collect();
            (all, None)
        }
    };
    let rows = table.rows();
    let mut out = Vec::new();
    for at in 0..rows.chunk_count() {
        let chunk = rows.read(at, &columns)?.settled()?;
        let chunk = match picked {
            Some(picked) => match picked.pick(chunk) {
                Some(kept) => kept,
                None => return Ok(None),
            },
            None => chunk,
        };
        for row in 0..chunk.len() {
            out.push(format!("{:?}", chunk.row(row).collect::<Vec<Value>>()));
        }
    }
    out.sort_unstable();
    Ok(Some(out))
}

/// Where the columns the read's scan read are in `table`, or `None` when one of them is not there
/// any more as it was.
fn columns(picked: &Picked, table: &Table) -> Option<Vec<usize>> {
    picked
        .fields
        .iter()
        .map(|field| {
            let at = table.column_index(&field.name)?;
            (table.columns()[at].ty == field.ty).then_some(at)
        })
        .collect()
}

impl Picked {
    /// The rows of `chunk`, which holds the scan's columns, that the filters keep, with the columns
    /// of the last of them. `None` when one of them fails on the rows.
    fn pick(&self, mut chunk: Chunk) -> Option<Chunk> {
        let mut schema = Schema::numbered(self.fields.clone(), self.index);
        for step in &self.steps {
            match step {
                Step::Keep(predicate) => {
                    let kept = rudb_exec::evaluate(&self.plan, *predicate, &schema, &chunk).ok()?;
                    let selection = Selection::from_predicate(chunk.len(), |row| {
                        kept.value_at(row) == Value::Boolean(true)
                    });
                    chunk = chunk.compact(&selection).ok()?;
                }
                Step::Make { index, exprs } => {
                    let columns = exprs
                        .iter()
                        .map(|&expr| rudb_exec::evaluate(&self.plan, expr, &schema, &chunk))
                        .collect::<Result<Vec<_>>>()
                        .ok()?;
                    let fields = exprs
                        .iter()
                        .map(|&expr| Field::new(String::new(), self.plan.expr_type(expr).clone()))
                        .collect();
                    chunk = Chunk::with_rows(columns, chunk.len()).ok()?;
                    schema = Schema::numbered(fields, *index);
                }
            }
        }
        Some(chunk)
    }
}

fn by_oid(catalog: &Catalog, oid: i64) -> Option<&Table> {
    catalog.tables().find(|table| table.oid() == oid)
}
