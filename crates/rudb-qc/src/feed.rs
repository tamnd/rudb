//! Running one pipeline as the steps of section 5.4 of `spec/compiler/05-pipelines-and-state.md`,
//! handing the body one chunk to a morsel.
//!
//! Only the body is generated in C1. The other steps a pipeline has are small and run here: init
//! writes the state header and points the state at what the runtime made for it, the join tables
//! its probes read included, and finalize reads an aggregate's groups out of its table or lays out
//! a join build's table for the pipelines that probe it. Every call of the body returns a
//! [`Status`] and [`Feed::push`] does what it asks.
//!
//! The body reads its columns through the morsel's column table, which wants a values address and
//! a validity bitmap per column. A fixed width column of a flat vector is already the first of
//! those and is passed as it is. A string column is not, because a `StringView` holds an offset
//! into its column's arena and compiled code wants an address it can read without knowing which
//! column the string came from, so each view is rewritten as a `str16` into a buffer that lives as
//! long as the call.
//!
//! A result body writes its rows into buffers the driver points it at, and they are turned into a
//! chunk straight after the call. The strings in them are copied out then too, because they point
//! into the chunk that was just read or into the runtime's heap, and neither is kept.
//!
//! Past a join probe one row can make many, so there the buffers start as long as the morsel and
//! the body says `NeedMemory` when they are full. The driver then doubles them and runs the morsel
//! again from the start, which is safe because the only thing such a body changes is the buffers
//! and their count, and both start over.

use std::fmt;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};

use rudb_common::{Cancel, Error, ErrorCode, Result};
use rudb_pipeline::{Lease, Progress, Sink};
use rudb_plan::Plan;
use rudb_qc_gen::{Body, Grouping, Out};
use rudb_qc_ir::status::{Kind, Status};
use rudb_qc_ir::{ErrorKind, Module};
use rudb_qc_pipe::{Pipeline, Source, Step};
use rudb_qc_plan::{Column, Key};
use rudb_qc_rt::abi::{Col, Morsel, StateHeader};
use rudb_qc_rt::table::{Agreed, Distinct, GroupTable, Job};
use rudb_qc_rt::{RUNTIME_ERROR, Rt, text};
use rudb_vector::{Chunk, Data, VECTOR_SIZE, Validity, Vector};

use crate::Under;
use crate::finish::{self, Cell, cell, vector};
use crate::merge;
use crate::tier::{self, Tiers};

/// For each worker table, the pairs of a worker group and the group it became in one part.
type Made = Vec<Vec<(u32, u32)>>;

/// One pipeline being run.
pub(crate) struct Feed<'a> {
    module: &'a Module,
    tiers: &'a Tiers,
    func: usize,
    /// The version of the body for a morsel with no NULL in the columns it reads.
    nonull: Option<usize>,
    /// The rows the pipeline's input has, when that is known, which the tier of a version is
    /// picked by.
    rows: Option<usize>,
    body: &'a Body,
    steps: Vec<Step>,
    columns: &'a [Column],
    cancel: Cancel,
    /// Whether the scan may run this pipeline on many workers at once, each with its own state and
    /// runtime, folded together as each one finishes.
    parallel: bool,
    /// The state as init left it, which a worker's starts as.
    template: Vec<Line>,
    /// How an aggregate's workers fold their groups together, and its distinct sets.
    folds: Vec<merge::Fold>,
    sets: Vec<u64>,
    /// Whether the workers are kept until the last one finishes and then merged a part of the
    /// groups to a thread, which a table with keys and no distinct sets is.
    split: bool,
    /// The keys and the row count of a top N that is all that reads this pipeline's rows, so a
    /// merge that makes its groups into chunks keeps only the rows of each chunk that could make
    /// it.
    top: Option<(&'a [Key], u64)>,
    /// The keys an aggregate's tables agree to make groups for, when a limit with no order is all
    /// that reads its groups.
    agreed: Option<Arc<Agreed>>,
    inner: Mutex<Inner<'a>>,
}

/// One worker of a parallel pipeline.
pub(crate) struct Worker {
    rt: Rt,
    state: Vec<Line>,
    /// The chunks a result body produced.
    out: Vec<Chunk>,
}

/// What a call changes.
struct Inner<'a> {
    rt: &'a mut Rt,
    /// The body's state, in cache lines so that the header is aligned as the spec lays it out.
    state: Vec<Line>,
    /// The chunks a result body produced.
    out: Vec<Chunk>,
    /// Whether the body said the pipeline may stop.
    done: bool,
    /// The runtimes of the workers that finished, when they are merged all at once.
    workers: Vec<Rt>,
    /// Whether an aggregate's groups were already made into chunks, which a merge does.
    grouped: bool,
    /// Whether a worker has been folded into the runtime yet.
    merged: bool,
}

/// One cache line of state.
#[repr(C, align(64))]
#[derive(Clone, Copy)]
struct Line([u8; 64]);

impl fmt::Debug for Feed<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Feed").field("func", &self.body.func).finish_non_exhaustive()
    }
}

impl<'a> Feed<'a> {
    /// A feed for pipeline `p`, whose generated body is `body`, with its init step run.
    pub(crate) fn new(
        module: &'a Module,
        tiers: &'a Tiers,
        p: &'a Pipeline,
        body: &'a Body,
        rt: &'a mut Rt,
        cancel: &Cancel,
        unordered: bool,
    ) -> Result<Feed<'a>> {
        let func = tiers
            .func(&body.func)
            .ok_or_else(|| Error::internal(format!("no function {} in the module", body.func)))?;
        let steps = p.steps();
        let columns = match &p.sink {
            rudb_qc_pipe::Sink::Result { columns, .. }
            | rudb_qc_pipe::Sink::Build { columns, .. }
            | rudb_qc_pipe::Sink::Aggregate { columns, .. } => columns.as_slice(),
        };
        let mut state = vec![Line([0; 64]); (body.state as usize).div_ceil(64).max(1)];
        // Init. With one worker the local state is the shared state, so the header points at its
        // own block, which never moves because the vector is never grown.
        head(&mut state);
        if let Out::Aggregate(g) = &body.sink
            && let Some(at) = g.row
        {
            // The one group of an aggregate with no groups was made with the table, and rows
            // never move, so its address is written once.
            let table = rt
                .table(g.table)
                .ok_or_else(|| Error::internal("the aggregate's table is not in the runtime"))?;
            let row = table.address(0) as u64;
            bytes(&mut state)[at as usize..at as usize + 8].copy_from_slice(&row.to_le_bytes());
        }
        for probe in &body.probes {
            // The build ran and was finalized before this pipeline started, and its table does
            // not move after that.
            let table = rt
                .join(probe.table)
                .filter(|t| t.is_finished())
                .ok_or_else(|| Error::internal("a probe of a join table that is not built"))?;
            let published = table.published();
            let st = bytes(&mut state);
            for (at, word) in [
                (probe.directory, published.directory as u64),
                (probe.shift, published.shift),
                (probe.tags, published.tags as u64),
            ] {
                st[at as usize..at as usize + 8].copy_from_slice(&word.to_le_bytes());
            }
        }
        let cancel = cancel.clone();
        let (parallel, folds, sets) = match &body.sink {
            Out::Aggregate(g) if matches!(p.source, Source::Scan { .. }) => {
                (true, merge::folds(g)?, merge::sets(g))
            }
            // Rows that only a sort reads may come from many workers in any order. A worker's
            // runtime has no join tables, so a body that probes one stays on one worker.
            Out::Result { .. }
                if unordered
                    && body.probes.is_empty()
                    && matches!(p.source, Source::Scan { .. }) =>
            {
                (true, Vec::new(), Vec::new())
            }
            _ => (false, Vec::new(), Vec::new()),
        };
        Ok(Feed {
            module,
            tiers,
            func,
            nonull: body.nonull.as_deref().and_then(|name| tiers.func(name)),
            rows: None,
            body,
            steps,
            columns,
            cancel,
            parallel,
            template: state.clone(),
            split: parallel
                && matches!(&body.sink, Out::Aggregate(g) if !g.keys.is_empty() || !sets.is_empty()),
            folds,
            sets,
            top: None,
            agreed: None,
            inner: Mutex::new(Inner {
                rt,
                state,
                out: Vec::new(),
                done: false,
                merged: false,
                workers: Vec::new(),
                grouped: false,
            }),
        })
    }

    /// A worker with a runtime of its own and its state as init leaves it.
    fn worker(&self) -> Worker {
        self.tiers.joined(self.func);
        if let Some(f) = self.nonull {
            self.tiers.joined(f);
        }
        let mut rt = self.lock().rt.worker();
        if self.split
            && let Out::Aggregate(g) = &self.body.sink
            && let Some(table) = rt.table_mut(g.table)
        {
            table.cap(WORKER_GROUPS);
        }
        if let (Some(agreed), Out::Aggregate(g)) = (&self.agreed, &self.body.sink)
            && let Some(table) = rt.table_mut(g.table)
        {
            table.limit(Arc::clone(agreed));
        }
        let mut state = self.template.clone();
        head(&mut state);
        if let Out::Aggregate(g) = &self.body.sink
            && let Some(at) = g.row
            && let Some(table) = rt.table(g.table)
        {
            // The worker's own one group, which its table made when it was made.
            let row = table.address(0) as u64;
            bytes(&mut state)[at as usize..at as usize + 8].copy_from_slice(&row.to_le_bytes());
        }
        Worker { rt, state, out: Vec::new() }
    }

    /// Says that the pipeline's input has `rows` rows, when that is known.
    pub(crate) fn sized(mut self, rows: Option<usize>) -> Self {
        self.rows = rows;
        self
    }

    /// Says that only a top N over `keys` of `count` rows, skipped ones included, reads what this
    /// pipeline produces.
    pub(crate) fn topped(mut self, keys: &'a [Key], count: u64) -> Self {
        self.top = Some((keys, count));
        self
    }

    /// Says that only a limit of `count` rows with no order, skipped ones included, reads the
    /// groups of this pipeline, so its tables make groups for the first `count` keys they see
    /// between them and no others. An aggregate with distinct sets is left as it is, because the
    /// rows of the keys left out would add to them.
    pub(crate) fn limited(mut self, count: usize) -> Self {
        let Out::Aggregate(g) = &self.body.sink else { return self };
        if g.keys.is_empty() || !merge::sets(g).is_empty() || count >= WORKER_GROUPS {
            return self;
        }
        let agreed = self.lock().rt.table_mut(g.table).map(|table| {
            let agreed = Arc::new(Agreed::new(table.layout().clone(), count));
            table.limit(Arc::clone(&agreed));
            agreed
        });
        self.agreed = agreed;
        self
    }

    /// Folds a worker that has seen its last morsel into the query's runtime. The first one is
    /// taken as it is, so a pipeline that ran on one worker folds nothing.
    fn fold(&self, mut worker: Worker) -> Result<()> {
        let mut inner = self.lock();
        let g = match &self.body.sink {
            Out::Aggregate(g) => g,
            Out::Result { .. } => {
                inner.out.append(&mut worker.out);
                inner.rt.retire(worker.rt);
                return Ok(());
            }
            Out::Build(_) => return Err(Error::internal("a parallel pipeline that builds")),
        };
        if self.split {
            inner.workers.push(worker.rt);
            Ok(())
        } else if inner.merged {
            inner
                .rt
                .absorb(worker.rt, g.table, &self.sets, |d, s| merge::fold(&self.folds, d, s))
                .map(drop)
        } else {
            inner.merged = true;
            inner.rt.adopt(worker.rt, g.table, &self.sets);
            Ok(())
        }
    }

    /// Merges the workers [`Feed::fold`] kept into the query's runtime, once the last one has
    /// finished. Each worker's groups are split by hash first, so that a key lands in the same part
    /// on every worker, and then each part is merged into a table of its own on a thread of its
    /// own. The parts have no key in common and are joined into one table by moving their pages.
    ///
    /// Folding the workers one after another under the lock took most of the time of a query
    /// with millions of groups, over a second of ClickBench q16's at sixteen threads.
    fn merge(&self, threads: &Lease<'_>) -> Result<()> {
        let Out::Aggregate(g) = &self.body.sink else {
            return Ok(());
        };
        let mut inner = self.lock();
        let mut workers = std::mem::take(&mut inner.workers);
        if workers.is_empty() {
            return Ok(());
        }
        // The distinct sets are merged on their own once the groups are, so they are taken out
        // of every worker first.
        let sets = self
            .sets
            .iter()
            .map(|&h| {
                workers.iter_mut().map(|w| w.take_distinct(h).ok_or_else(|| gone(h))).collect()
            })
            .collect::<Result<Vec<Vec<Distinct>>>>()?;
        // A worker that forgot its groups may have a key twice, which only the parts fold.
        let forgot = workers.iter().filter_map(|w| w.table(g.table)).any(GroupTable::forgot);
        let first = workers.remove(0);
        let folds = &self.folds;
        let fold = |d: &mut [u8], s: &[u8]| merge::fold(folds, d, s);
        let groups: usize =
            workers.iter().filter_map(|w| w.table(g.table)).map(GroupTable::len).sum();
        inner.rt.adopt(first, g.table, &[]);
        // The workers are folded into the adopted table, which must not forget them.
        if let Some(table) = inner.rt.table_mut(g.table) {
            table.cap(0);
        }
        // Enough parts that the slots of each fit in the cache of the thread folding it.
        let mine = inner.rt.table(g.table).map_or(0, GroupTable::len);
        let parts = ((groups + mine) / PART_GROUPS)
            .max(threads.degree() * 4)
            .next_power_of_two()
            .clamp(16, 4096);
        let bits = parts.trailing_zeros();
        // Which merged group each worker's group became, for the distinct sets.
        let mut maps: Vec<Vec<usize>> = Vec::with_capacity(workers.len() + 1);
        maps.push((0..inner.rt.table(g.table).map_or(0, GroupTable::len)).collect());
        // Few groups fold faster on one thread than they split.
        let split = !g.keys.is_empty() && (groups >= SPLIT_FROM || forgot);
        if split {
            let (merged, made) = {
                let mine = inner.rt.table(g.table).ok_or_else(|| gone(g.table))?;
                let tables = std::iter::once(Ok(mine))
                    .chain(workers.iter().map(|w| w.table(g.table).ok_or_else(|| gone(g.table))))
                    .collect::<Result<Vec<&GroupTable>>>()?;
                let splits = pieces(threads, tables.len(), |at| tables[at].split(bits))?;
                let layout = mine.layout().clone();
                // Folding a worker's row into a new group gives the row back, but for a distinct
                // set, which the fold leaves alone.
                let whole = sets.is_empty();
                let merged = pieces(threads, parts, |part| {
                    let most = splits.iter().map(|s| s[part].len()).sum();
                    let mut table = GroupTable::with_capacity(layout.clone(), most);
                    let mut made = vec![Vec::new(); if sets.is_empty() { 0 } else { tables.len() }];
                    for (t, (other, split)) in tables.iter().zip(&splits).enumerate() {
                        table.absorb_some(other, &split[part], made.get_mut(t), whole, fold);
                    }
                    table.seal();
                    (table, made)
                })?;
                let (merged, made): (Vec<GroupTable>, Vec<Made>) = merged.into_iter().unzip();
                // A group of a part is found at the part's first group and its place in it.
                let mut from = Vec::with_capacity(parts);
                let mut at = 0;
                for t in &merged {
                    from.push(at);
                    at += t.len();
                }
                let made = if sets.is_empty() {
                    Vec::new()
                } else {
                    pieces(threads, tables.len(), |t| {
                        let mut map = vec![0; tables[t].len()];
                        for (part, made) in made.iter().enumerate() {
                            for &(gid, local) in &made[t] {
                                map[gid as usize] = from[part] + local as usize;
                            }
                        }
                        map
                    })?
                };
                let (joined, ran) = GroupTable::join_with(layout, merged, |jobs| {
                    let jobs: Vec<Mutex<Option<Job<'_>>>> =
                        jobs.into_iter().map(|job| Mutex::new(Some(job))).collect();
                    pieces(threads, jobs.len(), |at| {
                        if let Some(job) = jobs[at].lock().ok().and_then(|mut job| job.take()) {
                            job();
                        }
                    })
                });
                ran?;
                (joined, made)
            };
            maps = made;
            // The table adopted from the first worker is replaced, and its strings with it, but
            // its heap is kept with the worker, which the merged table's strings may point into.
            inner.rt.settle(workers, g.table, merged)?;
        } else {
            for w in workers {
                maps.push(inner.rt.absorb(w, g.table, &[], fold)?);
            }
        }
        let total = inner.rt.table(g.table).map_or(0, GroupTable::len);
        for (&h, list) in self.sets.iter().zip(&sets) {
            let splits = pieces(threads, list.len(), |w| list[w].split(&maps[w], bits))?;
            let counts: Vec<AtomicU64> = (0..total).map(|_| AtomicU64::new(0)).collect();
            pieces(threads, parts, |part| {
                let sources: Vec<(&Distinct, &[usize], &[u32])> = list
                    .iter()
                    .zip(&maps)
                    .zip(&splits)
                    .map(|((d, m), s)| (d, m.as_slice(), s[part].as_slice()))
                    .collect();
                Distinct::gather(&sources, &counts);
            })?;
            let counts = counts.into_iter().map(AtomicU64::into_inner).collect();
            inner.rt.put_distinct(h, Distinct::counted_only(counts))?;
        }
        if !split {
            return Ok(());
        }
        // The groups are made into chunks here too, while there are threads to do it with.
        let table = inner.rt.table(g.table).ok_or_else(|| gone(g.table))?;
        let sets = self
            .sets
            .iter()
            .map(|&h| inner.rt.distinct(h).map(|d| (h, d)).ok_or_else(|| gone(h)))
            .collect::<Result<Vec<_>>>()?;
        let (columns, top) = (self.columns, self.top);
        let span = span(top, g);
        let chunks = pieces(threads, table.len().div_ceil(span), |at| {
            piece(table, &sets, g, columns, top, at * span, ((at + 1) * span).min(table.len()))
        })?;
        let mut out = Vec::with_capacity(chunks.len());
        for chunk in chunks {
            out.append(&mut chunk?);
        }
        inner.out = out;
        inner.grouped = true;
        Ok(())
    }

    /// Runs the body over every chunk of a scan, by building `scan` in the first engine with this
    /// feed as the root. When `pruned` is set the root of `scan` is a filter that only picks the
    /// parts the scan reads, and the body runs the filter on the rows.
    pub(crate) fn scan(&self, scan: &Plan, pruned: bool, under: Under<'_>) -> Result<()> {
        let sink = Arc::new(Scan(self));
        let build =
            if pruned { rudb_exec::build_pruned_into } else { rudb_exec::build_measured_into };
        let query = build(
            scan,
            under.catalog,
            under.cancel,
            under.memory,
            under.seams,
            under.session,
            sink,
        )?;
        query.run(under.cancel, under.pool)
    }

    /// Runs the body over one chunk, and says whether the pipeline wants more.
    pub(crate) fn push(&self, chunk: &Chunk) -> Result<Progress> {
        let mut inner = self.lock();
        if inner.done {
            return Ok(Progress::Done);
        }
        let Inner { rt, state, out, done, .. } = &mut *inner;
        self.run(chunk, rt, state, out, done)
    }

    /// Runs the body over one chunk against `rt` and `state`.
    fn run(
        &self,
        chunk: &Chunk,
        rt: &mut Rt,
        state: &mut [Line],
        out: &mut Vec<Chunk>,
        done: &mut bool,
    ) -> Result<Progress> {
        let chunk = chunk.clone().settled()?.into_flat()?;
        let rows = chunk.len();
        if rows == 0 {
            return Ok(Progress::More);
        }
        let mut held = Vec::with_capacity(self.body.reads.len());
        for &c in &self.body.reads {
            held.push(Held::of(chunk.column(c)?, rows)?);
        }
        let cols: Vec<Col> = held.iter().map(Held::col).collect();
        let mut morsel = Morsel {
            source: 0,
            chunk: 0,
            begin: 0,
            end: u32::try_from(rows)
                .map_err(|_| Error::internal("a chunk too long for a morsel"))?,
            seq: 0,
            enc: 0,
            flags: 0,
            cols: cols.as_ptr(),
        };
        let mut room = rows;
        let mut buffers = Vec::new();
        let sink = match &self.body.sink {
            Out::Result { .. } => tier::Sink::Result,
            Out::Aggregate(_) => tier::Sink::Aggregate,
            Out::Build(_) => tier::Sink::Build,
        };
        // A morsel with no NULL in what the body reads runs the version that checks none, which is
        // a guard checked before the call and so never sent back.
        let mut f = self.func;
        if let Some(v) = self.nonull
            && held.iter().all(|h| h.clean)
            && self.tiers.speculate(v)
        {
            self.tiers.prepare(self.module, v, self.rows, !self.body.probes.is_empty());
            f = v;
        }
        let start = self.tiers.start(f);
        'attempt: loop {
            buffers.clear();
            if let Out::Result { count, columns, capacity } = &self.body.sink {
                let st = bytes(state);
                st[*count as usize..*count as usize + 8].fill(0);
                if let Some(at) = capacity {
                    st[*at as usize..*at as usize + 8]
                        .copy_from_slice(&(room as u64).to_le_bytes());
                }
                for slot in columns {
                    let mut b = Buffers {
                        values: vec![0u128; (room * slot.ty.bytes() as usize).div_ceil(16)],
                        valid: vec![0u8; room],
                    };
                    let values = b.values.as_mut_ptr() as u64;
                    let valid = b.valid.as_mut_ptr() as u64;
                    st[slot.values as usize..slot.values as usize + 8]
                        .copy_from_slice(&values.to_le_bytes());
                    st[slot.valid as usize..slot.valid as usize + 8]
                        .copy_from_slice(&valid.to_le_bytes());
                    buffers.push(b);
                }
            }
            let st = state.as_mut_ptr().cast::<u8>();
            // A chunk is cut into morsels of `split` rows when that is set, all of them run on the
            // same version, and a result's rows go on where the morsel before left them.
            let step = match self.tiers.split() {
                0 => rows,
                n => n.min(rows),
            };
            let mut begin = 0;
            while begin < rows {
                let end = (begin + step).min(rows);
                (morsel.begin, morsel.end) = (begin as u32, end as u32);
                begin = end;
                // The tier is picked once per morsel, so a switch only ever happens between two.
                let native = self.tiers.morsel(f, sink);
                loop {
                    let status = self.tiers.call(native, f, st, (&raw const morsel).cast(), rt);
                    match Status(status).kind() {
                        Kind::Ok => break,
                        // The body saved where it got to in the header's cursor and picks up there.
                        Kind::Yield => {}
                        Kind::Done => {
                            *done = true;
                            break 'attempt;
                        }
                        // Only a result sink past a probe asks, and only when its buffers are full.
                        Kind::NeedMemory
                            if matches!(self.body.sink, Out::Result { capacity: Some(_), .. }) =>
                        {
                            room = room.checked_mul(2).ok_or_else(|| {
                                Error::internal("a join made more rows than memory holds")
                            })?;
                            continue 'attempt;
                        }
                        // A guard failed inside the body. Only a result sink starts the morsel over
                        // from nothing, so only there is it run again on the guard's fallback.
                        Kind::Deopt if matches!(self.body.sink, Out::Result { .. }) => {
                            let fallback = usize::try_from(Status(status).payload())
                                .ok()
                                .and_then(|at| self.module.guards.get(at))
                                .and_then(|g| self.tiers.func(&g.fallback))
                                .filter(|&back| back != f)
                                .ok_or_else(|| self.check(Status(status), rt))?;
                            self.tiers.deopt(f);
                            self.tiers.prepare(self.module, fallback, self.rows, false);
                            f = fallback;
                            continue 'attempt;
                        }
                        _ => return Err(self.check(Status(status), rt)),
                    }
                }
            }
            break;
        }
        drop(held);
        if let Some(start) = start {
            self.tiers.ran(self.module, f, rows, start);
        }
        if let Out::Result { count, columns, .. } = &self.body.sink {
            let st = bytes(state);
            let n = u64::from_le_bytes(
                st[*count as usize..*count as usize + 8].try_into().unwrap_or_default(),
            );
            let n = usize::try_from(n).unwrap_or(usize::MAX).min(room);
            let mut vectors = Vec::with_capacity(columns.len());
            for (slot, b) in columns.iter().zip(&buffers) {
                let w = slot.ty.bytes() as usize;
                // SAFETY: the buffer is `rows * w` bytes long and was allocated as `u128`s, which
                // any byte pattern is.
                let values = unsafe {
                    std::slice::from_raw_parts(b.values.as_ptr().cast::<u8>(), b.values.len() * 16)
                };
                let cells: Vec<Cell> = (0..n)
                    .map(|i| (b.valid[i] != 0).then(|| cell(&values[i * w..(i + 1) * w])))
                    .collect();
                vectors.push(vector(&slot.logical, &cells)?);
            }
            out.push(Chunk::with_rows(vectors, n)?);
        }
        Ok(if *done { Progress::Done } else { Progress::More })
    }

    /// Runs the steps after the body once every chunk has been pushed, and returns the rows the
    /// pipeline produced.
    pub(crate) fn finish(self) -> Result<Vec<Chunk>> {
        let inner = self.inner.into_inner().map_err(|_| Error::internal("a feed was poisoned"))?;
        let mut out = inner.out;
        for step in &self.steps {
            match step {
                Step::Init | Step::Body => {}
                // The accumulators of an aggregate with no groups are the one row of its table,
                // so there is nothing kept aside to flush.
                Step::LocalFin => {}
                Step::Merge => return Err(Error::internal("a merge step with one worker")),
                Step::Finalize => match &self.body.sink {
                    Out::Aggregate(g) if !inner.grouped => {
                        let table = inner.rt.table(g.table).ok_or_else(|| gone(g.table))?;
                        let sets = finish::sets(inner.rt, g)?;
                        let span = span(self.top, g);
                        let mut cuts = Vec::new();
                        for from in (0..table.len()).step_by(span) {
                            let to = (from + span).min(table.len());
                            cuts.append(&mut piece(
                                table,
                                &sets,
                                g,
                                self.columns,
                                self.top,
                                from,
                                to,
                            )?);
                        }
                        out = cuts;
                    }
                    Out::Aggregate(_) => {}
                    // The build publishes its table to the probes and produces no rows.
                    Out::Build(b) => {
                        inner.rt.finish_join(b.table)?;
                        out = Vec::new();
                    }
                    Out::Result { .. } => {}
                },
            }
        }
        Ok(out)
    }

    fn lock(&self) -> MutexGuard<'_, Inner<'a>> {
        self.inner.lock().unwrap_or_else(|held| held.into_inner())
    }

    /// The error a status that stops the pipeline stands for.
    fn check(&self, s: Status, rt: &mut Rt) -> Error {
        match s.kind() {
            Kind::Cancelled => match self.cancel.check() {
                Err(e) => e,
                Ok(()) => Error::interrupt("Interrupted!"),
            },
            Kind::Error if s.payload() == RUNTIME_ERROR => rt
                .take_error()
                .unwrap_or_else(|| Error::internal("the runtime failed and did not say why")),
            Kind::Error => {
                let Some(site) =
                    usize::try_from(s.payload()).ok().and_then(|at| self.module.errors.get(at))
                else {
                    return Error::internal(format!("status {s:?} names no error site"));
                };
                let code = match site.kind {
                    ErrorKind::Overflow | ErrorKind::DivideByZero | ErrorKind::OutOfRange => {
                        ErrorCode::OutOfRange
                    }
                    ErrorKind::Conversion => ErrorCode::Conversion,
                    ErrorKind::Cancel => ErrorCode::Interrupt,
                    ErrorKind::Internal => return Error::internal(site.text.clone()),
                };
                Error::new(code, site.text.clone())
            }
            // A guard that fails in a body that cannot start its morsel over, or names no
            // fallback, and a body that grows that is not a result sink past a probe, are bugs.
            Kind::Deopt => Error::internal(format!("a pipeline deoptimized at {s:?}")),
            Kind::NeedMemory => Error::internal(format!("a pipeline asked for memory, {s:?}")),
            _ => Error::internal(format!("a pipeline returned status {s:?}")),
        }
    }
}

/// How many groups [`piece`] takes at once: many when a top N by a count reads them, because
/// then only the groups that can make it are made into values, and a chunk's worth otherwise.
fn span(top: Option<(&[Key], u64)>, g: &Grouping) -> usize {
    match top {
        Some((keys, _)) if finish::counted(g, keys).is_some() => COUNTED_SPAN,
        _ => VECTOR_SIZE,
    }
}

/// The groups from `from` to `to` of `table` as chunks, cut to the top N when only a top N reads
/// them.
fn piece(
    table: &GroupTable,
    sets: &[(u64, &Distinct)],
    g: &Grouping,
    columns: &[Column],
    top: Option<(&[Key], u64)>,
    from: usize,
    to: usize,
) -> Result<Vec<Chunk>> {
    let kept = top.and_then(|(keys, count)| {
        let at = finish::counted(g, keys)?;
        finish::counted_top(table, at, keys.len() > 1, count, from, to)
    });
    let gids = kept.unwrap_or_else(|| (from..to).collect());
    let mut out = Vec::new();
    for gids in gids.chunks(VECTOR_SIZE) {
        out.append(&mut cut(finish::group_rows(table, sets, g, columns, gids)?, top)?);
    }
    Ok(out)
}

/// A chunk of groups cut to its top N, when only a top N reads them. Every row of the answer is in
/// the top N of the chunk it is in.
fn cut(chunk: Chunk, top: Option<(&[Key], u64)>) -> Result<Vec<Chunk>> {
    match top {
        Some((keys, count)) if (count as usize) < chunk.len() => {
            finish::sort(vec![chunk], keys, Some(count), 0)
        }
        _ => Ok(vec![chunk]),
    }
}

/// The output buffers of one result column for one call.
struct Buffers {
    values: Vec<u128>,
    valid: Vec<u8>,
}

/// Writes the state header, which points at the state's own block.
fn head(state: &mut [Line]) {
    let header = StateHeader::new(state.as_ptr().cast());
    // SAFETY: the first line of the state is 64 bytes aligned to 64, which is the header.
    unsafe { state.as_mut_ptr().cast::<StateHeader>().write(header) };
}

/// The state as bytes.
fn bytes(state: &mut [Line]) -> &mut [u8] {
    // SAFETY: a `Line` is 64 bytes with no padding and any byte pattern is one.
    unsafe { std::slice::from_raw_parts_mut(state.as_mut_ptr().cast::<u8>(), state.len() * 64) }
}

/// A source column as the body reads it, and whatever had to be made for that.
struct Held<'c> {
    values: *const u8,
    valid: Vec<u8>,
    /// Whether the column has no NULL in the morsel.
    clean: bool,
    /// The `str16` headers of a string column, which `values` points at.
    _text: Vec<u128>,
    _chunk: std::marker::PhantomData<&'c Vector>,
}

impl<'c> Held<'c> {
    fn of(v: &'c Vector, rows: usize) -> Result<Held<'c>> {
        let mut valid = vec![0xffu8; rows.div_ceil(8)];
        let mut clean = true;
        if !matches!(v.validity(), Validity::AllValid) {
            valid.fill(0);
            for i in 0..rows {
                if v.is_null_at(i) {
                    clean = false;
                } else {
                    valid[i / 8] |= 1 << (i % 8);
                }
            }
        }
        let data = v.data().ok_or_else(|| Error::internal("a flattened column is not flat"))?;
        let mut text = Vec::new();
        let values = match data {
            Data::Empty => {
                text = vec![0u128; rows];
                text.as_ptr().cast::<u8>()
            }
            Data::Bool(b) => b.as_slice().as_ptr().cast(),
            Data::Int8(b) => b.as_slice().as_ptr().cast(),
            Data::Int16(b) => b.as_slice().as_ptr().cast(),
            Data::Int32(b) => b.as_slice().as_ptr().cast(),
            Data::Int64(b) => b.as_slice().as_ptr().cast(),
            Data::Int128(b) => b.as_slice().as_ptr().cast(),
            Data::UInt8(b) => b.as_slice().as_ptr().cast(),
            Data::UInt16(b) => b.as_slice().as_ptr().cast(),
            Data::UInt32(b) => b.as_slice().as_ptr().cast(),
            Data::UInt64(b) => b.as_slice().as_ptr().cast(),
            Data::UInt128(b) => b.as_slice().as_ptr().cast(),
            Data::Float32(b) => b.as_slice().as_ptr().cast(),
            Data::Float64(b) => b.as_slice().as_ptr().cast(),
            Data::Varlen(s) => {
                let arena = s.arena();
                text = s
                    .views()
                    .iter()
                    .map(|view| match view.bytes_in(arena) {
                        Some(b) => text::make(b),
                        None => text::make(&[]),
                    })
                    .collect();
                text.as_ptr().cast::<u8>()
            }
            _ => return Err(Error::internal("a column of a type the generator refuses")),
        };
        Ok(Held { values, valid, clean, _text: text, _chunk: std::marker::PhantomData })
    }

    fn col(&self) -> Col {
        Col { values: self.values, valid: self.valid.as_ptr() }
    }
}

/// The root of a scan the first engine runs for us.
struct Scan<'f, 'a>(&'f Feed<'a>);

impl fmt::Debug for Scan<'_, '_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_tuple("Scan").field(self.0).finish()
    }
}

impl Sink for Scan<'_, '_> {
    type Local = Option<Worker>;

    fn local(&self) -> Self::Local {
        self.0.parallel.then(|| self.0.worker())
    }

    fn parallel(&self) -> bool {
        self.0.parallel
    }

    fn sink(&self, chunk: &Chunk, local: &mut Self::Local) -> Result<Progress> {
        match local {
            Some(w) => self.0.run(chunk, &mut w.rt, &mut w.state, &mut w.out, &mut false),
            None => self.0.push(chunk),
        }
    }

    fn combine(&self, local: Self::Local) -> Result<()> {
        match local {
            Some(w) => self.0.fold(w),
            None => Ok(()),
        }
    }

    fn finalize(&self, threads: &Lease<'_>) -> Result<()> {
        if self.0.split { self.0.merge(threads) } else { Ok(()) }
    }
}

/// How many groups the workers of an aggregate have between them before they are merged in parts.
const SPLIT_FROM: usize = 1 << 16;

/// About how many groups one part of a merge holds, so that its slots stay in the cache.
const PART_GROUPS: usize = 1 << 14;

/// How many groups [`piece`] takes at once when it can pick the ones a top N keeps by their count.
const COUNTED_SPAN: usize = 1 << 16;

/// How many groups a worker's table holds before it forgets them and starts again, so that its
/// slots and the rows it is filling stay in the cache.
const WORKER_GROUPS: usize = 1 << 15;

fn gone(table: u64) -> Error {
    Error::internal(format!("the group table or distinct set {table} is gone"))
}

/// Runs `count` pieces of work on the lease's threads, the calling one included, and hands back
/// what they made in order. A thread takes the next piece off a counter when it finishes one,
/// because the pieces are not the same size.
fn pieces<T: Send>(
    threads: &Lease<'_>,
    count: usize,
    run: impl Fn(usize) -> T + Sync,
) -> Result<Vec<T>> {
    let next = AtomicUsize::new(0);
    let slots: Vec<Mutex<Option<T>>> = (0..count).map(|_| Mutex::new(None)).collect();
    let step = || {
        loop {
            let at = next.fetch_add(1, Ordering::Relaxed);
            if at >= count {
                return;
            }
            let made = run(at);
            if let Ok(mut slot) = slots[at].lock() {
                *slot = Some(made);
            }
        }
    };
    let ((), panicked) = threads.scatter_at_most(count, &step, step);
    if panicked {
        return Err(Error::internal("a thread merging an aggregate panicked"));
    }
    slots
        .into_iter()
        .map(|slot| {
            slot.into_inner()
                .ok()
                .flatten()
                .ok_or_else(|| Error::internal("a piece of a merge went missing"))
        })
        .collect()
}
