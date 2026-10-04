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
use rudb_common::{Error, Result};
use rudb_vector::Chunk;

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
        (self.written.entry(oid).or_insert_with(Written::new), base)
    }

    /// The frame and the row count of the table `oid` in the snapshot, if it was there.
    pub(crate) fn before(&self, oid: i64) -> Option<(u64, u64)> {
        by_oid(&self.base, oid).map(|table| (table.frame(), table.rows().len() as u64))
    }
}

/// What a transaction did to the rows of one table.
#[derive(Debug, Default)]
pub(crate) struct Written {
    /// For each row of the transaction's copy of the table, the snapshot's number for it, or
    /// `u64::MAX` for a row the transaction added. `None` until the transaction deletes a row, and
    /// until then a row below the snapshot's count keeps its number and a row past it is new.
    origin: Option<Vec<u64>>,
    /// Every change in order, or `None` once the transaction did something to the table that is not
    /// an append or a change of rows it can name, after which the commit cannot do it again.
    changes: Option<Vec<Change>>,
}

impl Written {
    /// Whether everything done to the table was adding rows.
    fn appends(&self) -> bool {
        self.changes
            .as_ref()
            .is_some_and(|changes| changes.iter().all(|change| matches!(change, Change::Insert(_))))
    }

    pub(crate) fn new() -> Self {
        Self { origin: None, changes: Some(Vec::new()) }
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

    /// Notes rows appended.
    pub(crate) fn appended(&mut self, chunks: &[Chunk]) {
        let added = chunks.iter().map(Chunk::len).sum::<usize>();
        if let Some(origin) = self.origin.as_mut() {
            origin.extend(std::iter::repeat_n(u64::MAX, added));
        }
        // Onto the insert before it when there is one, so a load of one row at a time keeps one
        // list of rows rather than a change for each.
        if let Some(changes) = self.changes.as_mut() {
            if let Some(Change::Insert(last)) = changes.last_mut() {
                last.extend(chunks.iter().cloned());
            } else {
                changes.push(Change::Insert(chunks.to_vec()));
            }
        }
    }

    /// Notes the rows at `rows` of the transaction's copy given new values, which are the rows of
    /// `new` in the same order.
    pub(crate) fn updated(&mut self, rows: &[u64], new: &[Chunk]) {
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
        self.changes = None;
    }
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
                let changes = written.changes.ok_or_else(commit_conflict)?;
                return rebase(committed, &name, before, changes, workers);
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
            let changes = written.changes.ok_or_else(commit_conflict)?;
            rebase(&mut next, &name, before, changes, workers)?;
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
/// changed since the snapshot's `before`.
fn rebase(
    committed: &mut Catalog,
    name: &QualifiedName,
    before: &Table,
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
        return committed.table_mut(name)?.append_committing(chunks, workers).map_err(|error| {
            if error.code() == rudb_common::ErrorCode::Constraint {
                Error::transaction(format!("Failed to commit: {}", error.message()))
            } else {
                error
            }
        });
    }
    // Row numbers only mean the same rows while nothing committed since moved a row or added one,
    // since the transaction's own appends were numbered from the snapshot's end.
    let now = committed.table(name)?;
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
