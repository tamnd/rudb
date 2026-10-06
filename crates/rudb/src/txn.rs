//! Transactions, one per connection.
//!
//! A transaction that has touched the database works on its own copy of the catalog, taken when it
//! first read or wrote anything, which is when the pin takes its snapshot too. A copy is cheap
//! because a table's rows are chunks that are shared rather than copied, and only the tables the
//! transaction writes get new ones. Its statements read and write that copy, so nothing it does is
//! seen by another connection until it commits, and nothing another connection commits is seen by
//! it at all.
//!
//! Two transactions that write the same row are told apart when the second one writes it, which is
//! when the pin tells them apart. Every transaction that has written a row it did not add says which
//! rows in the [`Registry`], numbered the way its snapshot numbers them, and a write of one of them
//! from anywhere else fails with the pin's conflict text. So does a write of a row somebody changed
//! and committed after the writer's snapshot, which is what keeps this snapshot isolation rather than
//! just locking.
//!
//! The commit puts the transaction's copy back into the committed catalog, in one of three ways.
//! When nothing was committed since the snapshot, the copy is the committed catalog from then on.
//! When other commits changed other tables, the tables this one wrote are put in and theirs are
//! kept. When they changed a table this one wrote too, what this one did to it is done again to the
//! committed table: its appends go on the end, and its updates and deletes land on the same rows, as
//! long as nothing committed since has moved any row to a new number. Anything else fails the commit.
//!
//! A write that meets a row another open transaction holds may wait for it to end rather than fail,
//! for as long as `lock_timeout` says, and [`Registry::may_wait`] says when by wait-die, the rule
//! section 8.4 of the concurrency spec gives: a writer that holds nothing may wait for anyone, an
//! older transaction waits for a younger one, and anything else fails at once. Every wait then goes
//! from older to younger or from a writer nobody can be waiting on, so no cycle forms. When the
//! holder rolls back the write goes ahead, and when it commits the write meets a row committed
//! after its snapshot and fails the way it would have without waiting.
//!
//! This is the step before `engine-v4/08-concurrency.md`'s design, which keeps one copy of each row
//! with an undo chain and a lock word in it, and the conflicts it reports are the same ones.

use std::collections::{BTreeSet, HashMap};
use std::sync::{Condvar, Mutex, MutexGuard, PoisonError};
use std::time::Instant;

use rudb_catalog::{Catalog, QualifiedName, Table};
use rudb_common::{Error, Field, LogicalType, Result, Value};
use rudb_vector::vector::VECTOR_SIZE;
use rudb_vector::{Chunk, Selection, Vector};

use crate::journal::Change;

/// A transaction `BEGIN` opened and nothing has closed yet.
#[derive(Debug)]
pub(crate) struct Open {
    /// Whether a statement failed inside it, after which only `COMMIT` and `ROLLBACK` run and both
    /// of them roll back, which is what the pin does.
    pub(crate) aborted: bool,
    /// Whether it was begun `READ ONLY`.
    pub(crate) read_only: bool,
    /// What it took when it first touched the database, and what it has written since.
    pub(crate) snapshot: Option<Snapshot>,
}

impl Open {
    pub(crate) fn new(read_only: bool) -> Self {
        Self { aborted: false, read_only, snapshot: None }
    }
}

/// The part of a transaction that exists once it has touched the database.
#[derive(Debug)]
pub(crate) struct Snapshot {
    /// Its number in the [`Registry`].
    pub(crate) id: u64,
    /// The committed catalog as it was when the transaction took its copy.
    pub(crate) base: Catalog,
    /// The last revision handed out when it did, so a change committed afterwards can be told from
    /// one committed before.
    pub(crate) at: u64,
    /// What it did to each table it wrote rows of, by oid.
    pub(crate) written: HashMap<i64, Written>,
}

impl Snapshot {
    /// What the transaction wrote to the table `oid`, and how many rows the table had in the
    /// snapshot, none for a table the transaction created.
    pub(crate) fn written(&mut self, oid: i64) -> (&mut Written, u64) {
        let base = by_oid(&self.base, oid).map_or(0, |table| table.rows().len() as u64);
        (self.written.entry(oid).or_default(), base)
    }

    /// The frame and the row count of the table `oid` in the snapshot, if it was there.
    pub(crate) fn before(&self, oid: i64) -> Option<(u64, u64)> {
        by_oid(&self.base, oid).map(|table| (table.frame(), table.rows().len() as u64))
    }
}

/// What a transaction did to the rows of one table.
#[derive(Debug)]
pub(crate) struct Written {
    /// For each row of the transaction's copy of the table, the snapshot's number for it, or
    /// `u64::MAX` for a row the transaction added. `None` until the transaction deletes a row, and
    /// until then a row below the snapshot's count keeps its number and a row past it is new.
    origin: Option<Vec<u64>>,
    /// Every change in order, or `None` once the transaction did something to the table that is not
    /// an append or a change of rows it can name, after which the commit cannot do it again.
    changes: Option<Vec<Change>>,
    /// Rows appended as values since the last change in `changes`, which come after it.
    ///
    /// A prepared insert hands its rows over as values, and building them into a chunk for each
    /// statement cost a vector for every column of every row, for a list the commit reads only
    /// when somebody else committed to the table since the snapshot. So they are kept as they are
    /// and built into chunks, a vector's worth of rows at a time, when a change of another kind
    /// comes after them or when the commit asks.
    pending: Vec<Vec<Value>>,
    /// The types of the columns of the rows in `pending`.
    types: Vec<LogicalType>,
}

/// A table the transaction has not written yet: no row deleted, and no change that the commit
/// cannot do again.
impl Default for Written {
    fn default() -> Self {
        Self { origin: None, changes: Some(Vec::new()), pending: Vec::new(), types: Vec::new() }
    }
}

impl Written {
    /// Whether everything done to the table was adding rows.
    fn appends(&self) -> bool {
        self.changes
            .as_ref()
            .is_some_and(|changes| changes.iter().all(|change| matches!(change, Change::Insert(_))))
    }

    /// The snapshot's numbers for the rows at `rows` in the transaction's copy, leaving out the
    /// rows the transaction added. `base` is how many rows the table had in the snapshot.
    pub(crate) fn based(&self, rows: &[u64], base: u64) -> BTreeSet<u64> {
        match &self.origin {
            None => rows.iter().copied().filter(|&row| row < base).collect(),
            Some(origin) => rows
                .iter()
                .filter_map(|&row| origin.get(row as usize).copied())
                .filter(|&row| row != u64::MAX)
                .collect(),
        }
    }

    /// Notes rows appended, keeping the chunks themselves rather than a copy, since a copy of a
    /// chunk is a copy of every value in it and the caller has no more use for them.
    pub(crate) fn appended(&mut self, chunks: Vec<Chunk>) {
        self.build_pending();
        let added = chunks.iter().map(Chunk::len).sum::<usize>();
        if let Some(origin) = self.origin.as_mut() {
            origin.extend(std::iter::repeat_n(u64::MAX, added));
        }
        self.inserted(chunks);
    }

    /// Notes rows appended as values, each one value for each of `fields`, of its field's type or
    /// one that widens to it.
    pub(crate) fn appended_values(&mut self, fields: &[Field], rows: Vec<Vec<Value>>) {
        if let Some(origin) = self.origin.as_mut() {
            origin.extend(std::iter::repeat_n(u64::MAX, rows.len()));
        }
        if self.changes.is_none() {
            return;
        }
        if self.pending.is_empty() {
            self.types = fields.iter().map(|field| field.ty.clone()).collect();
        }
        self.pending.extend(rows);
    }

    /// Onto the insert before it when there is one, so a load of one row at a time keeps one list
    /// of rows rather than a change for each.
    fn inserted(&mut self, chunks: Vec<Chunk>) {
        if let Some(changes) = self.changes.as_mut() {
            if let Some(Change::Insert(last)) = changes.last_mut() {
                last.extend(chunks);
            } else {
                changes.push(Change::Insert(chunks));
            }
        }
    }

    /// Builds the rows in `pending` into chunks and notes them as an insert. Rows that will not
    /// build, which a value of a type its column does not take would be, leave the commit unable
    /// to do the transaction again, and it fails as a conflict rather than writing anything else.
    fn build_pending(&mut self) {
        if self.pending.is_empty() {
            return;
        }
        let pending = std::mem::take(&mut self.pending);
        match chunks_of(&self.types, pending) {
            Ok(chunks) => self.inserted(chunks),
            Err(_) => self.changes = None,
        }
    }

    /// Every change in order, or `None` when the commit cannot do them again, for the commit.
    pub(crate) fn into_changes(mut self) -> Option<Vec<Change>> {
        self.build_pending();
        self.changes
    }

    /// Notes the rows at `rows` of the transaction's copy given new values, which are the rows of
    /// `new` in the same order.
    pub(crate) fn updated(&mut self, rows: &[u64], new: &[Chunk]) {
        self.build_pending();
        let Some(changes) = self.changes.as_mut() else { return };
        if new.iter().map(Chunk::len).sum::<usize>() != rows.len() {
            self.changes = None;
            return;
        }
        let mut at = 0;
        for chunk in new {
            let these = &rows[at..at + chunk.len()];
            at += chunk.len();
            changes.push(Change::Update(runs(these), vec![chunk.clone()]));
        }
    }

    /// Notes the rows at `rows` of the transaction's copy, which had `len` rows, taken out.
    pub(crate) fn deleted(&mut self, rows: &[u64], len: usize, base: u64) {
        self.build_pending();
        let origin = self.origin.get_or_insert_with(|| {
            (0..len as u64).map(|row| if row < base { row } else { u64::MAX }).collect()
        });
        let mut gone = rows.iter().copied().peekable();
        let mut at = 0u64;
        origin.retain(|_| {
            let keep = gone.next_if_eq(&at).is_none();
            at += 1;
            keep
        });
        if let Some(changes) = self.changes.as_mut() {
            changes.push(Change::Delete(runs(rows)));
        }
    }

    /// Notes a change the commit could not do again from rows.
    pub(crate) fn opaque(&mut self) {
        self.pending = Vec::new();
        self.changes = None;
    }
}

/// Rows of values as chunks of up to a vector's worth of rows each, the columns of the types
/// `types`.
fn chunks_of(types: &[LogicalType], rows: Vec<Vec<Value>>) -> Result<Vec<Chunk>> {
    let mut chunks = Vec::with_capacity(rows.len().div_ceil(VECTOR_SIZE));
    let mut rows = rows.into_iter().peekable();
    while rows.peek().is_some() {
        let mut columns: Vec<Vec<Value>> =
            types.iter().map(|_| Vec::with_capacity(VECTOR_SIZE)).collect();
        for row in rows.by_ref().take(VECTOR_SIZE) {
            if row.len() != types.len() {
                return Err(Error::internal(
                    "a row appended as values is not as wide as its table",
                ));
            }
            for (column, value) in columns.iter_mut().zip(row) {
                column.push(value);
            }
        }
        let vectors = types
            .iter()
            .zip(&columns)
            .map(|(ty, values)| Vector::from_values(ty.clone(), values))
            .collect::<Result<Vec<_>>>()?;
        chunks.push(Chunk::new(vectors)?);
    }
    Ok(chunks)
}

/// Sorted row numbers as runs of first row and length.
fn runs(rows: &[u64]) -> Vec<(u64, u64)> {
    let mut runs: Vec<(u64, u64)> = Vec::new();
    for &row in rows {
        match runs.last_mut() {
            Some((first, len)) if *first + *len == row => *len += 1,
            _ => runs.push((row, 1)),
        }
    }
    runs
}

/// Which rows the open transactions have written, and which rows were committed since the oldest
/// of them took its snapshot. One per database.
#[derive(Debug, Default)]
pub(crate) struct Registry {
    next: u64,
    open: HashMap<u64, Claims>,
    done: Vec<Done>,
}

/// The [`Registry`] and what a write waiting on one of its transactions sleeps on. One per
/// database.
#[derive(Debug, Default)]
pub(crate) struct Board {
    registry: Mutex<Registry>,
    /// Told whenever a transaction ends.
    ended: Condvar,
}

impl Board {
    pub(crate) fn lock(&self) -> MutexGuard<'_, Registry> {
        self.registry.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// [`Registry::end`], waking whoever waits for a transaction to end.
    pub(crate) fn end(&self, me: u64, committed: bool) {
        self.lock().end(me, committed);
        self.ended.notify_all();
    }

    /// Waits until transaction `holder` has ended or `deadline` has passed, and says which.
    pub(crate) fn wait_for(&self, holder: u64, deadline: Instant) -> bool {
        let mut registry = self.lock();
        while registry.open.contains_key(&holder) {
            let now = Instant::now();
            if now >= deadline {
                return false;
            }
            registry = self
                .ended
                .wait_timeout(registry, deadline - now)
                .unwrap_or_else(PoisonError::into_inner)
                .0;
        }
        true
    }
}

/// What a write met in [`Registry::clashes`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Clash {
    /// A row the open transaction of this id holds.
    Open(u64),
    /// A row committed after the writer's snapshot, which waiting cannot help.
    Done,
}

/// What one open transaction has written.
#[derive(Debug, Default)]
struct Claims {
    /// The revision its snapshot was taken at.
    at: u64,
    /// By table, updated and deleted rows together.
    tables: HashMap<i64, Marks>,
    /// The tables it created, whose names nobody else may create until it is done.
    creating: Vec<QualifiedName>,
    /// The tables it altered or made a trigger on, which nobody else may do either to until it is
    /// done.
    holding: Vec<QualifiedName>,
}

/// Rows of one table, numbered in one frame.
#[derive(Debug, Clone)]
pub(crate) struct Marks {
    /// The frame the numbers are in. See `Table::frame`.
    pub(crate) frame: u64,
    pub(crate) rows: BTreeSet<u64>,
    /// Every row, for a write that could not say which.
    pub(crate) all: bool,
}

impl Marks {
    /// Whether these rows and `other` share one.
    fn meets(&self, other: &Self) -> bool {
        if self.all || other.all {
            return true;
        }
        if self.frame != other.frame {
            return false;
        }
        let (small, large) =
            if self.rows.len() <= other.rows.len() { (self, other) } else { (other, self) };
        small.rows.iter().any(|row| large.rows.contains(row))
    }
}

/// Rows of a table a commit changed.
#[derive(Debug)]
struct Done {
    oid: i64,
    /// The revision the commit drew.
    at: u64,
    marks: Marks,
}

impl Registry {
    /// Numbers a transaction taking its snapshot at revision `at`.
    pub(crate) fn begin(&mut self, at: u64) -> u64 {
        self.next += 1;
        self.open.insert(self.next, Claims { at, ..Claims::default() });
        self.next
    }

    /// Whether a write of `marks` in the table `oid` meets a row another open transaction wrote, or
    /// a row committed after revision `since`. `me` is the transaction writing, if it is one.
    ///
    /// An update also meets a delete of the same row, and the other way round. The pin lets both
    /// commit and the update is lost when the delete commits last, so rudb diverges on purpose
    /// here (`18-compat.md` section 18.8, question 2).
    pub(crate) fn clashes(
        &self,
        me: Option<u64>,
        oid: i64,
        marks: &Marks,
        since: u64,
    ) -> Option<Clash> {
        let done = self
            .done
            .iter()
            .any(|done| done.oid == oid && done.at > since && done.marks.meets(marks));
        if done {
            return Some(Clash::Done);
        }
        // The oldest holder, since a writer older than that one is older than every holder.
        self.open
            .iter()
            .filter(|(id, _)| Some(**id) != me)
            .filter(|(_, claims)| claims.tables.get(&oid).is_some_and(|held| held.meets(marks)))
            .map(|(id, _)| *id)
            .min()
            .map(Clash::Open)
    }

    /// Whether writer `me`, a transaction or a statement outside one, may wait for `holder` rather
    /// than fail, by wait-die: when it holds nothing, or when it is the older of the two.
    pub(crate) fn may_wait(&self, me: Option<u64>, holder: u64) -> bool {
        let Some(me) = me else { return true };
        let holds_nothing = self
            .open
            .get(&me)
            .is_none_or(|claims| claims.tables.is_empty() && claims.creating.is_empty());
        holds_nothing || me < holder
    }

    /// Records that transaction `me` wrote `marks` in the table `oid`.
    pub(crate) fn mark(&mut self, me: u64, oid: i64, marks: Marks) {
        let Some(claims) = self.open.get_mut(&me) else { return };
        match claims.tables.get_mut(&oid) {
            Some(held) if held.frame == marks.frame => {
                held.all |= marks.all;
                held.rows.extend(marks.rows);
            }
            Some(held) => held.all = true,
            None => {
                claims.tables.insert(oid, marks);
            }
        }
    }

    /// Records rows a statement outside any transaction changed, for the open transactions whose
    /// snapshots are older than it.
    pub(crate) fn committed(&mut self, oid: i64, marks: Marks) {
        if !self.open.is_empty() {
            let at = rudb_catalog::next_revision();
            self.done.push(Done { oid, at, marks });
        }
    }

    /// Whether any transaction is open, which is when a write has to say which rows it changed.
    pub(crate) fn watched(&self) -> bool {
        !self.open.is_empty()
    }

    /// Whether another open transaction created a table of this name.
    pub(crate) fn creating(&self, me: Option<u64>, name: &QualifiedName) -> bool {
        self.open
            .iter()
            .filter(|(id, _)| Some(**id) != me)
            .any(|(_, claims)| claims.creating.iter().any(|held| same(held, name)))
    }

    /// Whether another open transaction altered this table or made a trigger on it.
    pub(crate) fn holding(&self, me: Option<u64>, name: &QualifiedName) -> bool {
        self.open
            .iter()
            .filter(|(id, _)| Some(**id) != me)
            .any(|(_, claims)| claims.holding.iter().any(|held| same(held, name)))
    }

    /// Records that transaction `me` altered this table or made a trigger on it.
    pub(crate) fn hold(&mut self, me: u64, name: QualifiedName) {
        if let Some(claims) = self.open.get_mut(&me) {
            claims.holding.push(name);
        }
    }

    /// Records that transaction `me` created a table of this name.
    pub(crate) fn create(&mut self, me: u64, name: QualifiedName) {
        if let Some(claims) = self.open.get_mut(&me) {
            claims.creating.push(name);
        }
    }

    /// Closes transaction `me`, keeping what it wrote for the transactions still open when it
    /// committed, and forgetting what nobody still open can need.
    pub(crate) fn end(&mut self, me: u64, committed: bool) {
        let Some(claims) = self.open.remove(&me) else { return };
        if committed && !self.open.is_empty() {
            let at = rudb_catalog::next_revision();
            for (oid, marks) in claims.tables {
                self.done.push(Done { oid, at, marks });
            }
        }
        match self.open.values().map(|claims| claims.at).min() {
            Some(oldest) => self.done.retain(|done| done.at > oldest),
            None => self.done.clear(),
        }
    }
}

fn same(a: &QualifiedName, b: &QualifiedName) -> bool {
    rudb_catalog::same_name(&a.catalog, &b.catalog)
        && rudb_catalog::same_name(&a.schema, &b.schema)
        && rudb_catalog::same_name(&a.table, &b.table)
}

/// The pin's text for a write of a row somebody else wrote first.
pub(crate) fn conflict(delete: bool) -> Error {
    Error::transaction(if delete { "Conflict on tuple deletion!" } else { "Conflict on update!" })
}

/// The pin's text for a table two transactions both created.
pub(crate) fn create_conflict(name: &QualifiedName) -> Error {
    Error::transaction(format!("Catalog write-write conflict on create with \"{}\"", name.table))
}

/// The pin's text for an alter of a table another open transaction changed in the catalog.
pub(crate) fn alter_conflict(name: &QualifiedName) -> Error {
    Error::transaction(format!("Catalog write-write conflict on alter with \"{}\"", name.table))
}

/// A commit that met a change committed since its snapshot that it cannot be put together with.
fn commit_conflict() -> Error {
    Error::transaction("Failed to commit: another transaction changed what this one changed")
}

fn by_oid(catalog: &Catalog, oid: i64) -> Option<&Table> {
    catalog.tables().find(|table| table.oid() == oid)
}

/// Puts a committing transaction's catalog into the committed one. See the module comment for the
/// three ways, and for what fails.
///
/// # Errors
///
/// A conflict, or a key the transaction added that a commit since added too, in the pin's words for
/// each. The committed catalog is left as it was.
pub(crate) fn merge(
    committed: &mut Catalog,
    mine: Catalog,
    snapshot: &mut Snapshot,
    workers: usize,
) -> Result<()> {
    let base = &snapshot.base;
    if committed.generation() == base.generation() {
        committed.install(mine);
        return Ok(());
    }
    if mine.shape() == base.shape() {
        // One table changed, which is what an autocommit write is, lands on the committed catalog
        // where it is: the table itself when nobody else changed it since, and the rows added to
        // it otherwise. Every check comes before the first row goes in, so there is no second
        // table for a failure to leave half done and no copy of the catalog to take.
        let mut changed = mine.tables().filter(|table| {
            by_oid(base, table.oid()).is_none_or(|before| table.revision() != before.revision())
        });
        if let (Some(table), None) = (changed.next(), changed.next())
            && let Some(before) = by_oid(base, table.oid())
            && let Some(now) = by_oid(committed, table.oid())
            && let Some(written) = snapshot.written.get(&table.oid())
        {
            let name = now.name().clone();
            if now.revision() == before.revision() {
                *committed.table_mut(&name)? = table.clone();
                return Ok(());
            }
            if written.appends() {
                let written = snapshot.written.remove(&table.oid()).expect("asked just above");
                let changes = written.into_changes().ok_or_else(commit_conflict)?;
                return rebase(committed, &name, before, table, changes, workers);
            }
        }
        // Only rows changed here, so the committed catalog keeps its shape and takes this
        // transaction's tables. Worked on a copy, so a conflict in the second table leaves the
        // first one as it was.
        let mut next = committed.clone();
        for table in mine.tables() {
            let Some(before) = by_oid(base, table.oid()) else { return Err(commit_conflict()) };
            // A table handed out to be changed and given back as it was, which is what a `CHECK`
            // and a `RETURNING` do, has a new revision and no record of anything written.
            if table.revision() == before.revision() {
                continue;
            }
            let Some(written) = snapshot.written.remove(&table.oid()) else { continue };
            let Some(now) = by_oid(&next, table.oid()) else { return Err(commit_conflict()) };
            let name = now.name().clone();
            if now.revision() == before.revision() {
                *next.table_mut(&name)? = table.clone();
                continue;
            }
            let changes = written.into_changes().ok_or_else(commit_conflict)?;
            rebase(&mut next, &name, before, table, changes, workers)?;
        }
        *committed = next;
        return Ok(());
    }
    if committed.shape() == base.shape() {
        // The schema changed here and only rows changed there, so this transaction's catalog is
        // the committed one from now on, with the committed rows of every table it did not write.
        let mut mine = mine;
        for table in committed.tables() {
            let Some(before) = by_oid(base, table.oid()) else { return Err(commit_conflict()) };
            if table.revision() == before.revision() {
                continue;
            }
            let Some(held) = by_oid(&mine, table.oid()) else { return Err(commit_conflict()) };
            if snapshot.written.contains_key(&table.oid()) {
                return Err(commit_conflict());
            }
            let name = held.name().clone();
            *mine.table_mut(&name)? = table.clone();
        }
        committed.install(mine);
        return Ok(());
    }
    Err(commit_conflict())
}

/// Does what a transaction did to one table again, on the committed table `name`, which others
/// changed since the snapshot's `before`. `mine` is the transaction's copy of the table.
///
/// Appends go on the end. When nobody moved a row since, the snapshot's rows have the same numbers
/// in the committed table as in the snapshot, and the copy's numbers for them follow from the rows
/// the transaction took out. So the rows the transaction updated are written over where they are,
/// the rows it took out are taken out, and the rows it added, which are the rows of the copy past
/// the snapshot's, go on the end after whatever others added. None of it reads a row the
/// transaction did not write, unless the table keeps its rows in memory and the transaction took
/// one out. This is what lets a transaction commit while others append to the table, as long as
/// it did not write or take out a row it added itself, since the log numbers that row as the copy
/// did.
///
/// Anything else is done again on every row of the table read into memory, as long as the table
/// still has the snapshot's rows and no more.
fn rebase(
    committed: &mut Catalog,
    name: &QualifiedName,
    before: &Table,
    mine: &Table,
    changes: Vec<Change>,
    workers: usize,
) -> Result<()> {
    let appends = changes.iter().all(|change| matches!(change, Change::Insert(_)));
    if appends {
        let chunks = changes
            .into_iter()
            .filter_map(|change| match change {
                Change::Insert(chunks) => Some(chunks),
                _ => None,
            })
            .flatten()
            .collect::<Vec<_>>();
        let table = committed.table_appending(name)?;
        return table.append_committing(chunks, workers).map_err(failed_commit);
    }
    let base = before.rows().len() as u64;
    let now = committed.table(name)?;
    let others = now.rows().len() as u64 != base;
    if now.frame() == before.frame()
        && now.rows().len() as u64 >= base
        && let Some(done) = Redone::of(&changes, base)
        // The log has the transaction's update or delete of a row it added under the copy's
        // number for it, which names somebody else's row once others added rows before it.
        && !(others && done.own)
        && mine.rows().len() as u64 >= base - done.deleted.len() as u64
    {
        let kept = base - done.deleted.len() as u64;
        let numbers = done.updated.into_iter().collect::<Vec<_>>();
        let rows = picked(mine, &copied(&numbers, &done.deleted))?;
        let own = picked(mine, &(kept..mine.rows().len() as u64).collect::<Vec<_>>())?;
        // Where the keys are stays built across all three, see `Table::remove_rows`.
        let table = committed.table_appending(name)?;
        if !table.put_rows(&numbers, &rows)? {
            return Err(commit_conflict());
        }
        table.remove_rows(&done.deleted, workers)?;
        if !own.is_empty() {
            table.append_committing(own, workers).map_err(failed_commit)?;
        }
        return Ok(());
    }
    // Row numbers only mean the same rows while nothing committed since moved a row or added one,
    // since the transaction's own appends were numbered from the snapshot's end.
    if now.frame() != before.frame() || now.rows().len() != before.rows().len() {
        return Err(commit_conflict());
    }
    let fields = now.columns().to_vec();
    let rows = now.rows();
    let all = (0..fields.len()).collect::<Vec<_>>();
    let mut chunks = (0..rows.chunk_count())
        .map(|at| rows.read(at, &all).and_then(Chunk::settled))
        .collect::<Result<Vec<_>>>()?;
    let moves = changes.iter().any(|change| matches!(change, Change::Delete(_)));
    for change in changes {
        change.apply(&fields, &mut chunks)?;
    }
    let table = committed.table_mut(name)?;
    if moves { table.replace_all(chunks, workers) } else { table.update_all(chunks, workers) }
}

/// What a transaction's changes to one table did to the snapshot's rows, by the snapshot's numbers
/// for them.
#[derive(Debug, Default)]
struct Redone {
    /// The rows it took out, in order.
    deleted: Vec<u64>,
    /// The rows it wrote and did not take out afterwards.
    updated: BTreeSet<u64>,
    /// Whether it wrote or took out a row it added itself.
    own: bool,
}

impl Redone {
    /// Follows `changes` from a table of `base` rows, or `None` when they do not fit it.
    fn of(changes: &[Change], base: u64) -> Option<Self> {
        let mut done = Self::default();
        // The copy's rows from the snapshot come first, so a row of the copy is one of them while
        // its number is below how many of them are left.
        for change in changes {
            let (runs, deletes) = match change {
                Change::Insert(_) => continue,
                Change::Update(runs, _) => (runs, false),
                Change::Delete(runs) => (runs, true),
            };
            let left = base - done.deleted.len() as u64;
            let rows = runs.iter().flat_map(|&(first, len)| first..first + len);
            let (theirs, mine): (Vec<u64>, Vec<u64>) = rows.partition(|&row| row < left);
            done.own |= !mine.is_empty();
            let based = based(&theirs, &done.deleted)?;
            if deletes {
                for row in &based {
                    done.updated.remove(row);
                }
                done.deleted.extend(based);
                done.deleted.sort_unstable();
            } else {
                done.updated.extend(based);
            }
        }
        Some(done)
    }
}

/// The snapshot's numbers for the copy's rows `rows`, which rise and are all rows of the snapshot,
/// when the snapshot's rows `deleted`, which rise, are taken out of the copy. `None` if `rows` do
/// not rise.
fn based(rows: &[u64], deleted: &[u64]) -> Option<Vec<u64>> {
    let mut out = Vec::with_capacity(rows.len());
    let mut skipped = 0;
    let mut last = None;
    for &row in rows {
        if last.is_some_and(|last| row <= last) {
            return None;
        }
        last = Some(row);
        let mut number = row + skipped as u64;
        while deleted.get(skipped).is_some_and(|&gone| gone <= number) {
            skipped += 1;
            number += 1;
        }
        out.push(number);
    }
    Some(out)
}

/// The copy's numbers for the snapshot's rows `numbers`, none of them in `deleted`, both rising.
fn copied(numbers: &[u64], deleted: &[u64]) -> Vec<u64> {
    let mut skipped = 0;
    numbers
        .iter()
        .map(|&number| {
            while deleted.get(skipped).is_some_and(|&gone| gone < number) {
                skipped += 1;
            }
            number - skipped as u64
        })
        .collect()
}

/// A key the rows a commit adds repeat, in the words the pin fails a commit with.
fn failed_commit(error: Error) -> Error {
    if error.code() == rudb_common::ErrorCode::Constraint {
        Error::transaction(format!("Failed to commit: {}", error.message()))
    } else {
        error
    }
}

/// Every column of the rows of `table` at `numbers`, which rise, a chunk for each chunk of the
/// table they are in.
fn picked(table: &Table, numbers: &[u64]) -> Result<Vec<Chunk>> {
    let rows = table.rows();
    let all = (0..table.columns().len()).collect::<Vec<_>>();
    let mut out = Vec::new();
    let (mut at, mut start) = (0, 0_u64);
    for chunk in 0..rows.chunk_count() {
        if at == numbers.len() {
            break;
        }
        let end = start + rows.chunk_len(chunk)? as u64;
        let first = at;
        while numbers.get(at).is_some_and(|&number| number < end) {
            at += 1;
        }
        if at > first {
            let picks = numbers[first..at].iter().map(|&number| (number - start) as u32).collect();
            let read = rows.read(chunk, &all)?.settled()?;
            out.push(read.compact(&Selection::from_indices(picks))?);
        }
        start = end;
    }
    if at != numbers.len() {
        return Err(Error::internal("a row a transaction wrote is past its table"));
    }
    Ok(out)
}
