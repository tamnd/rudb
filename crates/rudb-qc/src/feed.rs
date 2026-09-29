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

use std::collections::HashMap;
use std::fmt;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};

use rudb_common::{Cancel, Error, ErrorCode, LogicalType, Result};
use rudb_exec::TopCut;
use rudb_pipeline::{Lease, Morsel as Cut, Progress, Sink};
use rudb_plan::Plan;
use rudb_qc_gen::{Body, Grouping, Out};
use rudb_qc_ir::status::{Kind, Status};
use rudb_qc_ir::{ErrorKind, Module};
use rudb_qc_pipe::{Pipeline, Source, Step};
use rudb_qc_plan::{Column, Key};
use rudb_qc_rt::abi::{Col, Morsel, StateHeader};
use rudb_qc_rt::like::Like;
use rudb_qc_rt::table::{Agreed, Distinct, GroupTable, Job, LANE_BITS, SetPart};
use rudb_qc_rt::{Ablate, RUNTIME_ERROR, Rt, text};
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
    /// Whether the rows of a result body are put back in the order of the morsels they came from
    /// once its workers finish, because nothing after it sorts them.
    ordered: bool,
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
    /// Where the rows of a result body tell the scan under them how good a row has to be to make
    /// the top N that reads them.
    cutoff: Option<TopCut>,
    /// For each column of [`Body::domains`], the index of each of its values.
    lookups: Vec<HashMap<Vec<u8>, u16>>,
    inner: Mutex<Inner<'a>>,
}

/// One worker of a parallel pipeline.
pub(crate) struct Worker {
    rt: Rt,
    state: Vec<Line>,
    /// The chunks a result body produced.
    out: Vec<Chunk>,
    /// The morsel the worker is on, and the morsel each chunk in `out` came from.
    morsel: u64,
    from: Vec<u64>,
    headers: Headers,
}

/// What a call changes.
struct Inner<'a> {
    rt: &'a mut Rt,
    /// The body's state, in cache lines so that the header is aligned as the spec lays it out.
    state: Vec<Line>,
    /// The chunks a result body produced.
    out: Vec<Chunk>,
    /// The morsel each chunk in `out` came from, when the workers made them.
    from: Vec<u64>,
    /// Whether the body said the pipeline may stop.
    done: bool,
    /// The runtimes of the workers that finished, when they are merged all at once.
    workers: Vec<Rt>,
    /// Whether an aggregate's groups were already made into chunks, which a merge does.
    grouped: bool,
    /// Whether a worker has been folded into the runtime yet.
    merged: bool,
    headers: Headers,
}

/// For each column the body reads, the last text dictionary it came with, that dictionary's values
/// made flat, and a `str16` header per value.
///
/// A text column storage keeps coded reaches the body as codes into one dictionary for the whole
/// column, or one a page. Made flat, every row's string was copied out of the dictionary into an
/// arena of its own and then made into a header, which was over a third of the instructions of
/// TPC-H q1 compiled. Kept coded, the dictionary is made into headers once and a row costs the
/// load of its header.
///
/// The same goes for each `LIKE` the body reads over a coded column: it is answered once for each
/// value of the dictionary, and a row costs the load of its code's answer.
#[derive(Debug, Default)]
struct Headers {
    text: Vec<Option<Flat>>,
    likes: Vec<Option<Answered>>,
    /// For each column of [`Body::domains`], the last dictionary it came with and the index of
    /// each of that dictionary's values, as [`indexes`] makes them.
    domains: Vec<Option<(Arc<Vector>, Vec<u16>)>>,
    /// `0, 1, 2, ...`, the codes a column made flat is read through, as long as the longest chunk
    /// so far.
    rows: Vec<u32>,
}

/// A dictionary, its values made flat, and a `str16` header per value.
type Flat = (Arc<Vector>, Arc<Vector>, Vec<u128>);

/// A dictionary and a `LIKE`'s answer for each of its values.
type Answered = (Arc<Vector>, Vec<u8>);

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
            // The rows of a result come from many workers, and are put back in the order of the
            // morsels unless only a sort reads them. A worker's runtime has no join tables, so a
            // body that probes one stays on one worker.
            Out::Result { .. }
                if body.probes.is_empty() && matches!(p.source, Source::Scan { .. }) =>
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
            ordered: parallel && !unordered && matches!(body.sink, Out::Result { .. }),
            template: state.clone(),
            split: parallel
                && matches!(&body.sink, Out::Aggregate(g) if !g.keys.is_empty() || !sets.is_empty()),
            folds,
            sets,
            top: None,
            agreed: None,
            cutoff: None,
            lookups: body
                .domains
                .iter()
                .map(|d| d.values.iter().enumerate().map(|(i, v)| (v.clone(), i as u16)).collect())
                .collect(),
            inner: Mutex::new(Inner {
                rt,
                state,
                out: Vec::new(),
                from: Vec::new(),
                done: false,
                merged: false,
                workers: Vec::new(),
                grouped: false,
                headers: Headers::default(),
            }),
        })
    }

    /// Whether a top N by a count is cut from the group rows before they are made into values.
    fn counting(&self) -> bool {
        !self.tiers.ablate().off(Ablate::TOP)
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
            if self.sets.is_empty() && !self.tiers.ablate().off(Ablate::LANES) {
                table.lanes();
            }
        }
        if self.split {
            for &h in &self.sets {
                if let Some(set) = rt.distinct_mut(h) {
                    set.spill();
                }
            }
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
        Worker {
            rt,
            state,
            out: Vec::new(),
            morsel: 0,
            from: Vec::new(),
            headers: Headers::default(),
        }
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

    /// Says that the top N that reads this pipeline's rows can tell the scan under it to skip the
    /// parts with no row good enough to make it, through `cut`. Each chunk the body makes is then
    /// cut to the top N, and the worst key kept is what the scan is told.
    pub(crate) fn telling(mut self, cut: Option<TopCut>) -> Self {
        self.cutoff =
            cut.filter(|_| self.top.is_some() && matches!(self.body.sink, Out::Result { .. }));
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
                inner.from.append(&mut worker.from);
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
            // Folding a worker's row into a new group gives the row back, but for a distinct set,
            // which the fold leaves alone, so without one a new group keeps its worker's row where
            // it is.
            if sets.is_empty() {
                return self.merge_whole(inner, workers, bits, threads);
            }
            let (merged, made) = {
                let mine = inner.rt.table(g.table).ok_or_else(|| gone(g.table))?;
                let tables = std::iter::once(Ok(mine))
                    .chain(workers.iter().map(|w| w.table(g.table).ok_or_else(|| gone(g.table))))
                    .collect::<Result<Vec<&GroupTable>>>()?;
                let splits = pieces(threads, tables.len(), |at| tables[at].split(bits))?;
                let layout = mine.layout().clone();
                let merged = pieces(threads, parts, |part| {
                    let most = splits.iter().map(|s| s[part].len()).sum();
                    let mut table = GroupTable::with_capacity(layout.clone(), most);
                    let mut made = vec![Vec::new(); tables.len()];
                    for (t, (other, split)) in tables.iter().zip(&splits).enumerate() {
                        table.absorb_some(other, &split[part], made.get_mut(t), false, fold);
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
                let made = pieces(threads, tables.len(), |t| {
                    let mut map = vec![0; tables[t].len()];
                    for (part, made) in made.iter().enumerate() {
                        for &(gid, local) in &made[t] {
                            map[gid as usize] = from[part] + local as usize;
                        }
                    }
                    map
                })?;
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
            // Parts of their own, few enough pairs each that the repeats are found in the cache.
            let held: usize = list.iter().map(Distinct::held).sum();
            let parts = (held / PART_PAIRS).next_power_of_two().clamp(16, 4096);
            let bits = parts.trailing_zeros();
            let splits = pieces(threads, list.len(), |w| list[w].split(&maps[w], bits))?;
            let counts: Vec<AtomicU64> = (0..total).map(|_| AtomicU64::new(0)).collect();
            pieces(threads, parts, |part| {
                let sources: Vec<SetPart<'_>> = list
                    .iter()
                    .zip(&maps)
                    .zip(&splits)
                    .map(|((d, m), s)| {
                        (d, m.as_slice(), s.pairs[part].as_slice(), s.picks[part].as_slice())
                    })
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
        let (columns, top, counting) = (self.columns, self.top, self.counting());
        let span = span(top, g, counting);
        let chunks = pieces(threads, table.len().div_ceil(span), |at| {
            let to = ((at + 1) * span).min(table.len());
            piece(table, &sets, g, columns, (top, counting), at * span, to)
        })?;
        let mut out = Vec::with_capacity(chunks.len());
        for chunk in chunks {
            out.append(&mut chunk?);
        }
        inner.out = out;
        inner.grouped = true;
        Ok(())
    }

    /// The rest of [`merge`](Feed::merge) for an aggregate with no distinct set, whose rows the
    /// merge takes where they are in `pages`. Each part folds its groups and makes them into chunks
    /// while they are in the cache of the thread that folded them, cut to the groups a top N by a
    /// count can keep when one reads them, so the parts are never joined into one table.
    fn merge_whole(
        &self,
        mut inner: MutexGuard<'_, Inner<'a>>,
        mut workers: Vec<Rt>,
        bits: u32,
        threads: &Lease<'_>,
    ) -> Result<()> {
        let Out::Aggregate(g) = &self.body.sink else {
            return Ok(());
        };
        let folds = &self.folds;
        let fold = |d: &mut [u8], s: &[u8]| merge::fold(folds, d, s);
        let (columns, top) = (self.columns, self.top);
        let counted = top
            .filter(|_| self.counting())
            .and_then(|(keys, count)| Some((finish::counted(g, keys)?, keys.len() > 1, count)));
        let (chunks, layout) = {
            let mine = inner.rt.table(g.table).ok_or_else(|| gone(g.table))?;
            let tables = std::iter::once(Ok(mine))
                .chain(workers.iter().map(|w| w.table(g.table).ok_or_else(|| gone(g.table))))
                .collect::<Result<Vec<&GroupTable>>>()?;
            // A worker's rows are already in a lane a part, which the part reads in order.
            let laned = tables.iter().all(|t| t.laned());
            let (bits, splits) = if laned {
                (LANE_BITS, Vec::new())
            } else {
                (bits, pieces(threads, tables.len(), |at| tables[at].split(bits))?)
            };
            let layout = mine.layout().clone();
            let chunks = pieces(threads, 1 << bits, |part| {
                let most = if laned {
                    tables.iter().map(|t| t.lane_len(part)).sum()
                } else {
                    splits.iter().map(|s| s[part].len()).sum()
                };
                let mut table = GroupTable::with_capacity(layout.clone(), most);
                // A top by a descending count is gathered as the lanes fold.
                let mut rising = counted
                    .filter(|_| laned)
                    .and_then(|(at, more, count)| finish::Rising::new(at, more, count));
                if laned {
                    for other in &tables {
                        table.absorb_lane(other, part, fold, |gid, row| {
                            if let Some(r) = &mut rising {
                                r.offer(gid, row);
                            }
                        });
                    }
                } else {
                    for (other, split) in tables.iter().zip(&splits) {
                        table.absorb_some(other, &split[part], None, true, fold);
                    }
                }
                table.seal();
                let gids = match rising {
                    Some(rising) => rising.finish(&table),
                    None => counted.and_then(|(at, more, count)| {
                        finish::counted_top(&table, at, more, count, 0, table.len())
                    }),
                }
                .unwrap_or_else(|| (0..table.len()).collect());
                let mut out = Vec::new();
                for gids in gids.chunks(VECTOR_SIZE) {
                    out.append(&mut cut(finish::group_rows(&table, &[], g, columns, gids)?, top)?);
                }
                Ok::<_, Error>(out)
            })?;
            (chunks, layout)
        };
        // The rows stay where they are until the query is done, as a merged table's would.
        let mut pages = Vec::new();
        if let Some(table) = inner.rt.table_mut(g.table) {
            pages.append(&mut table.take_pages());
        }
        for w in &mut workers {
            if let Some(table) = w.table_mut(g.table) {
                pages.append(&mut table.take_pages());
            }
        }
        let mut kept = GroupTable::new(layout);
        kept.keep(pages);
        inner.rt.settle(workers, g.table, kept)?;
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
    /// parts the scan reads, and the body runs the filter on the rows. The scan reads only the rows
    /// `handoffs` leave, which are the rows the joins the body probes can match.
    pub(crate) fn scan<'p>(
        &'p self,
        scan: &'p Plan,
        pruned: bool,
        under: Under<'p>,
        handoffs: &[rudb_exec::Handoff<'p>],
    ) -> Result<()> {
        let sink = Arc::new(Scan(self));
        let query = rudb_exec::build_handed_into(
            scan,
            under.catalog,
            under.cancel,
            under.memory,
            under.seams,
            under.session,
            sink,
            pruned,
            self.cutoff.as_ref(),
            handoffs,
        )?;
        query.run(under.cancel, under.pool)
    }

    /// Runs the body over one chunk, and says whether the pipeline wants more.
    pub(crate) fn push(&self, chunk: &Chunk) -> Result<Progress> {
        let mut inner = self.lock();
        if inner.done {
            return Ok(Progress::Done);
        }
        let Inner { rt, state, out, done, headers, .. } = &mut *inner;
        self.run(chunk, rt, state, out, done, headers)
    }

    /// Runs the body over one chunk against `rt` and `state`.
    fn run(
        &self,
        chunk: &Chunk,
        rt: &mut Rt,
        state: &mut [Line],
        out: &mut Vec<Chunk>,
        done: &mut bool,
        headers: &mut Headers,
    ) -> Result<Progress> {
        let chunk = chunk.clone().settled()?;
        let rows = chunk.len();
        if rows == 0 {
            return Ok(Progress::More);
        }
        // A `LIKE` over a column coded into a small dictionary, or one it has answered already, is
        // answered for the dictionary's values and read through the codes. Every other `LIKE` is
        // answered further down, over its column made flat.
        if headers.rows.len() < rows {
            let top = u32::try_from(rows).map_err(|_| Error::internal("a chunk too long"))?;
            headers.rows = (0..top).collect();
        }
        let each = headers.rows.as_ptr();
        headers.text.resize(self.body.reads.len(), None);
        headers.likes.resize(self.body.likes.len(), None);
        // The answers of a `LIKE` over a coded column, one a dictionary value.
        let mut answers: Vec<Option<*const u8>> = Vec::with_capacity(self.body.likes.len());
        for (m, known) in self.body.likes.iter().zip(headers.likes.iter_mut()) {
            let v = chunk.column(m.column)?;
            let codes = !self.tiers.ablate().off(Ablate::CODES);
            answers.push(if codes && worded(v, rows, known.as_ref()) {
                Some(like_coded(rt, m.like, v, known)?)
            } else {
                None
            });
        }
        let by_codes = |c: usize| {
            self.body.likes.iter().zip(&answers).any(|(m, a)| m.column == c && a.is_some())
        };
        let flat = |c: usize| {
            self.body.likes.iter().zip(&answers).any(|(m, a)| m.column == c && a.is_none())
        };
        // A text column the body reads is kept coded when its dictionary is small or its headers
        // are made already, and every other column is made flat. A column a `LIKE` answers over
        // its strings is made flat too, because the answers read its strings in place, and one
        // the body reads only through answers from its dictionary is not read at all.
        let mut keep = vec![false; chunk.width()];
        for (&c, known) in self.body.reads.iter().zip(headers.text.iter()) {
            keep[c] = !flat(c) && coded(chunk.column(c)?, rows, known.as_ref());
        }
        for m in &self.body.likes {
            if by_codes(m.column) && !flat(m.column) && !self.body.reads.contains(&m.column) {
                keep[m.column] = true;
            }
        }
        // A column a `LIKE` reads through its codes and the body reads made flat is kept coded
        // here too, so that its codes live as long as the morsel.
        let mut aside = Vec::new();
        let mut columns = Vec::with_capacity(keep.len());
        for (c, (v, &keep)) in chunk.into_columns().into_iter().zip(&keep).enumerate() {
            if keep {
                columns.push(v);
                continue;
            }
            if by_codes(c) {
                aside.push((c, v.clone()));
            }
            columns.push(v.into_flat()?);
        }
        let chunk = Chunk::with_rows(columns, rows)?;
        let coded_at = |c: usize| -> Result<&Vector> {
            match aside.iter().find(|(d, _)| *d == c) {
                Some((_, v)) => Ok(v),
                None => chunk.column(c),
            }
        };
        let mut held = Vec::with_capacity(self.body.reads.len());
        for (&c, known) in self.body.reads.iter().zip(headers.text.iter_mut()) {
            held.push(if keep[c] {
                Held::coded(chunk.column(c)?, rows, known)?
            } else {
                Held::of(chunk.column(c)?, rows, true, each)?
            });
        }
        let mut cols: Vec<Col> = held.iter().map(Held::col).collect();
        // Each other `LIKE` the body reads as a column is answered here for the whole morsel.
        let mut valids = Vec::new();
        let mut flats = Vec::new();
        for (m, answer) in self.body.likes.iter().zip(&answers) {
            let (values, codes) = match *answer {
                Some(values) => (values, codes_of(coded_at(m.column)?, rows)?),
                None => {
                    let like =
                        rt.like(m.like).ok_or_else(|| Error::internal("a LIKE pattern is gone"))?;
                    let mut out = vec![0u8; rows];
                    answer_flat(like, chunk.column(m.column)?, rows, &mut out);
                    flats.push(out);
                    (flats.last().map_or(std::ptr::null(), Vec::as_ptr), each)
                }
            };
            let valid = match self.body.reads.iter().position(|&c| c == m.column) {
                Some(at) => cols[at].valid,
                None => {
                    // The body reads the column only through its answers, so only its validity
                    // is held and not its strings.
                    let (valid, _) = validity(chunk.column(m.column)?, rows);
                    valids.push(valid);
                    valids.last().map_or(std::ptr::null(), Vec::as_ptr)
                }
            };
            cols.push(Col { values, valid, codes });
        }
        // Each group key column the body reads as indexes into its values is looked up here, a
        // dictionary's values once.
        headers.domains.resize(self.body.domains.len(), None);
        let mut found = Vec::with_capacity(self.body.domains.len());
        let domains = self.body.domains.iter().zip(&self.lookups);
        for ((d, lookup), known) in domains.zip(headers.domains.iter_mut()) {
            let (values, codes) = match indexes(coded_at(d.column)?, rows, lookup, known)? {
                Indexes::Coded(values, codes) => (values, codes),
                Indexes::Rows(out) => {
                    found.push(out);
                    (found.last().map_or(std::ptr::null(), Vec::as_ptr), each)
                }
            };
            let at = values.cast::<u8>();
            cols.push(Col { values: at, valid: at, codes });
        }
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
            && self
                .body
                .ranged
                .iter()
                .all(|&(c, k)| chunk.column(c).is_ok_and(|v| inside(v, rows, k)))
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
        drop(answers);
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
            let chunk = Chunk::with_rows(vectors, n)?;
            match (&self.cutoff, self.top) {
                (Some(cut), Some((keys, count))) if count > 0 && n as u64 >= count => {
                    let mut kept = finish::sort(vec![chunk], keys, Some(count), 0)?;
                    let first = keys.first().and_then(|k| match k.expr.kind {
                        rudb_qc_plan::Kind::Column(c) => Some(c),
                        _ => None,
                    });
                    if let (Some(last), Some(c)) = (kept.last(), first)
                        && !last.is_empty()
                    {
                        cut.reached(&last.value_at(last.len() - 1, c));
                    }
                    out.append(&mut kept);
                }
                _ => out.push(chunk),
            }
        }
        Ok(if *done { Progress::Done } else { Progress::More })
    }

    /// Runs the steps after the body once every chunk has been pushed, and returns the rows the
    /// pipeline produced.
    pub(crate) fn finish(self) -> Result<Vec<Chunk>> {
        let counting = self.counting();
        let inner = self.inner.into_inner().map_err(|_| Error::internal("a feed was poisoned"))?;
        let mut out = inner.out;
        if self.ordered && inner.from.len() == out.len() {
            // A morsel is run by one worker from start to end, so a stable sort by morsel puts
            // the rows back in the order the scan cut them.
            let mut placed: Vec<(u64, Chunk)> = inner.from.into_iter().zip(out).collect();
            placed.sort_by_key(|&(morsel, _)| morsel);
            out = placed.into_iter().map(|(_, chunk)| chunk).collect();
        }
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
                        let span = span(self.top, g, counting);
                        let mut cuts = Vec::new();
                        for from in (0..table.len()).step_by(span) {
                            let to = (from + span).min(table.len());
                            cuts.append(&mut piece(
                                table,
                                &sets,
                                g,
                                self.columns,
                                (self.top, counting),
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
fn span(top: Option<(&[Key], u64)>, g: &Grouping, counting: bool) -> usize {
    match top {
        Some((keys, _)) if counting && finish::counted(g, keys).is_some() => COUNTED_SPAN,
        _ => VECTOR_SIZE,
    }
}

/// The groups from `from` to `to` of `table` as chunks, cut to the top N when only a top N reads
/// them, and cut by the count first when `counting` says the top N can be.
fn piece(
    table: &GroupTable,
    sets: &[(u64, &Distinct)],
    g: &Grouping,
    columns: &[Column],
    (top, counting): (Option<(&[Key], u64)>, bool),
    from: usize,
    to: usize,
) -> Result<Vec<Chunk>> {
    let kept = top.filter(|_| counting).and_then(|(keys, count)| {
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
    /// The index into `values` of each row.
    codes: *const u32,
    /// Whether the column has no NULL in the morsel.
    clean: bool,
    /// The `str16` headers of a string column, which `values` points at.
    _text: Vec<u128>,
    _chunk: std::marker::PhantomData<&'c Vector>,
}

impl<'c> Held<'c> {
    /// Holds `v`, and without the strings of a string column when `strings` is false, for a
    /// column the body reads only the validity of. `each` is `0, 1, 2, ...` for at least `rows`
    /// rows, since a value is held a row.
    fn of(v: &'c Vector, rows: usize, strings: bool, each: *const u32) -> Result<Held<'c>> {
        let (valid, clean) = validity(v, rows);
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
            Data::Varlen(_) if !strings => std::ptr::null(),
            Data::Varlen(s) => {
                let arena = s.arena();
                // A view is laid out as a `str16` is, but for the address of a long string, which
                // is its offset in the arena. So a view is taken as it is and a long one has the
                // arena's address added, and the views are only read one by one when one of them
                // points past the arena.
                let base = arena.as_ptr().expose_provenance() as u64;
                let mut end = 0u64;
                text = s
                    .views()
                    .iter()
                    .map(|view| {
                        let w = view.to_bits();
                        let n = u64::from(w as u32);
                        if n as usize > text::INLINE {
                            let at = (w >> 64) as u64;
                            end = end.max(at.saturating_add(n));
                            (w & u128::from(u64::MAX)) | (u128::from(base.wrapping_add(at)) << 64)
                        } else {
                            w
                        }
                    })
                    .collect();
                if end > arena.len() as u64 {
                    text = s
                        .views()
                        .iter()
                        .map(|view| text::make(view.bytes_in(arena).unwrap_or_default()))
                        .collect();
                }
                text.as_ptr().cast::<u8>()
            }
            _ => return Err(Error::internal("a column of a type the generator refuses")),
        };
        let codes = each;
        Ok(Held { values, valid, codes, clean, _text: text, _chunk: std::marker::PhantomData })
    }

    /// A text column held as codes into a dictionary and the headers `known` has for the
    /// dictionary, made first when it has them for another one. No row is copied: the body reads
    /// the header of a row's code, and every code is below the dictionary's length because the
    /// vector checks that when it is made.
    fn coded(v: &'c Vector, rows: usize, known: &mut Option<Flat>) -> Result<Held<'c>> {
        let (codes, dictionary) = v
            .shared_dictionary_parts()
            .ok_or_else(|| Error::internal("a coded column that is not a dictionary"))?;
        let codes = codes.get(..rows).ok_or_else(|| Error::internal("fewer codes than rows"))?;
        if !known.as_ref().is_some_and(|(d, ..)| Arc::ptr_eq(d, dictionary)) {
            // The headers of long values point into `values`, which is kept with them.
            let values = Arc::new((**dictionary).clone().into_flat()?);
            let Some(Data::Varlen(s)) = values.data() else {
                return Err(Error::internal("a text dictionary whose values are not strings"));
            };
            let arena = s.arena();
            let made =
                s.views().iter().map(|view| text::make(view.bytes_in(arena).unwrap_or_default()));
            let made = made.collect();
            *known = Some((Arc::clone(dictionary), values, made));
        }
        let Some((_, _, made)) = known.as_ref() else {
            return Err(Error::internal("headers made and then not there"));
        };
        let (valid, clean) = validity(v, rows);
        let values = made.as_ptr().cast::<u8>();
        let codes = codes.as_ptr();
        let text = Vec::new();
        Ok(Held { values, valid, codes, clean, _text: text, _chunk: std::marker::PhantomData })
    }

    fn col(&self) -> Col {
        Col { values: self.values, valid: self.valid.as_ptr(), codes: self.codes }
    }
}

/// Whether `v` is read as codes into its dictionary, which is when it is text coded into a
/// dictionary with no NULL in it and either its headers are made already or the dictionary is not
/// much longer than the chunk, so that making them is no more work than copying out the rows.
fn coded(v: &Vector, rows: usize, known: Option<&Flat>) -> bool {
    if v.logical_type() != &LogicalType::Varchar {
        return false;
    }
    let Some((codes, dictionary)) = v.shared_dictionary_parts() else {
        return false;
    };
    codes.len() >= rows
        && !dictionary.validity().has_nulls(dictionary.len())
        && (known.is_some_and(|(d, ..)| Arc::ptr_eq(d, dictionary))
            || dictionary.len() <= rows.max(VECTOR_SIZE))
}

/// Whether a `LIKE` over `v` is answered for its dictionary's values rather than its rows, which is
/// when it is text coded into a dictionary and either the answers are there already or the
/// dictionary is not longer than the chunk, so that answering it is no more work than the rows.
fn worded(v: &Vector, rows: usize, known: Option<&Answered>) -> bool {
    if v.logical_type() != &LogicalType::Varchar {
        return false;
    }
    let Some((codes, dictionary)) = v.shared_dictionary_parts() else {
        return false;
    };
    codes.len() >= rows
        && (known.is_some_and(|(d, _)| Arc::ptr_eq(d, dictionary))
            || dictionary.len() <= rows.max(VECTOR_SIZE))
}

/// A `LIKE` over a coded column: the answers `known` has for each value of the dictionary, made
/// first when it has them for another one, which the body reads through the column's codes.
fn like_coded(rt: &Rt, like: u64, v: &Vector, known: &mut Option<Answered>) -> Result<*const u8> {
    let (_, dictionary) = v
        .shared_dictionary_parts()
        .ok_or_else(|| Error::internal("a coded column that is not a dictionary"))?;
    if !known.as_ref().is_some_and(|(d, _)| Arc::ptr_eq(d, dictionary)) {
        let like = rt.like(like).ok_or_else(|| Error::internal("a LIKE pattern is gone"))?;
        let values = (**dictionary).clone().into_flat()?;
        let mut out = vec![0u8; values.len()];
        answer_flat(like, &values, values.len(), &mut out);
        *known = Some((Arc::clone(dictionary), out));
    }
    let Some((_, made)) = known.as_ref() else {
        return Err(Error::internal("answers made and then not there"));
    };
    Ok(made.as_ptr())
}

/// The first `rows` codes of the coded column `v`.
fn codes_of(v: &Vector, rows: usize) -> Result<*const u32> {
    let (codes, _) = v
        .shared_dictionary_parts()
        .ok_or_else(|| Error::internal("a coded column that is not a dictionary"))?;
    let codes = codes.get(..rows).ok_or_else(|| Error::internal("fewer codes than rows"))?;
    Ok(codes.as_ptr())
}

/// A `LIKE` over the first `rows` strings of a flat text column, one answer a row, with a row that
/// is not a string answered false.
fn answer_flat(like: &Like, v: &Vector, rows: usize, out: &mut [u8]) {
    if let Some(Data::Varlen(s)) = v.data() {
        let (views, arena) = (s.views(), s.arena());
        // A long string's view holds its length and its offset in the arena.
        let place = |at: usize| {
            let w = views[at].to_bits();
            let n = w as u32 as usize;
            let start = (w >> 64) as u64 as usize;
            (n > text::INLINE).then(|| start..start.saturating_add(n))
        };
        let text = |at: usize| views[at].bytes_in(arena).unwrap_or_default();
        like.answer(rows, arena, place, text, out);
    }
}

/// Whether each of the first `rows` values of `v` is at least `-2^k` and under `2^k`, the range the
/// statistics gave the version of a body that leaves out the overflow checks that range rules out.
///
/// Adding `2^k` moves the range to `0..2^(k+1)`, so a value is in it when the sum has no bit at or
/// above `k + 1`, and or-ing the sums together asks that of every value at once. The loop has no
/// branch in it, so the compiler makes it vector adds and ors.
fn inside(v: &Vector, rows: usize, k: u32) -> bool {
    macro_rules! fits {
        ($values:expr, $signed:ty, $unsigned:ty) => {{
            if k + 1 >= <$unsigned>::BITS {
                return true;
            }
            let Some(values) = $values.as_slice().get(..rows) else { return false };
            let off: $signed = 1 << k;
            let all =
                values.iter().fold(0, |all, &x: &$signed| all | x.wrapping_add(off) as $unsigned);
            all >> (k + 1) == 0
        }};
    }
    match v.data() {
        Some(Data::Int8(b)) => fits!(b, i8, u8),
        Some(Data::Int16(b)) => fits!(b, i16, u16),
        Some(Data::Int32(b)) => fits!(b, i32, u32),
        Some(Data::Int64(b)) => fits!(b, i64, u64),
        Some(Data::Int128(b)) => fits!(b, i128, u128),
        _ => false,
    }
}

/// The validity bitmap of the first `rows` rows of `v`, and whether none of them is NULL.
fn validity(v: &Vector, rows: usize) -> (Vec<u8>, bool) {
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
    (valid, clean)
}

/// The index in `lookup` of each of the first `rows` values of the text column `v`, as the body
/// reads a column of [`Body::domains`]: the count of values for a NULL, and one more than that for
/// a value not in them. For a column coded into a dictionary, the dictionary's values are looked
/// up once and kept in `known` for the next morsel that comes with the same one, and when no row
/// is NULL the body reads them through the codes.
fn indexes(
    v: &Vector,
    rows: usize,
    lookup: &HashMap<Vec<u8>, u16>,
    known: &mut Option<(Arc<Vector>, Vec<u16>)>,
) -> Result<Indexes> {
    let n = lookup.len() as u16;
    let each = |flat: &Vector| -> Vec<u16> {
        // A value that is not a string is not in the values, and the hash finds its group.
        let Some(Data::Varlen(s)) = flat.data() else { return vec![n + 1; flat.len()] };
        let arena = s.arena();
        let at = |bytes: &[u8]| lookup.get(bytes).copied().unwrap_or(n + 1);
        s.views().iter().map(|view| at(view.bytes_in(arena).unwrap_or_default())).collect()
    };
    let mut out = match v.shared_dictionary_parts() {
        Some((codes, dictionary)) if codes.len() >= rows => {
            if !known.as_ref().is_some_and(|(d, _)| Arc::ptr_eq(d, dictionary)) {
                let values = (**dictionary).clone().into_flat()?;
                let mut map = each(&values);
                for (i, m) in map.iter_mut().enumerate() {
                    if values.is_null_at(i) {
                        *m = n;
                    }
                }
                *known = Some((Arc::clone(dictionary), map));
            }
            let Some((_, map)) = known.as_ref() else {
                return Err(Error::internal("indexes made and then not there"));
            };
            if matches!(v.validity(), Validity::AllValid) {
                return Ok(Indexes::Coded(map.as_ptr(), codes.as_ptr()));
            }
            codes[..rows].iter().map(|&c| map.get(c as usize).copied().unwrap_or(n + 1)).collect()
        }
        _ => {
            let mut out = match v.data() {
                Some(_) => each(v),
                None => each(&v.clone().into_flat()?),
            };
            out.resize(rows, n + 1);
            out
        }
    };
    if !matches!(v.validity(), Validity::AllValid) {
        for (i, x) in out.iter_mut().enumerate() {
            if v.is_null_at(i) {
                *x = n;
            }
        }
    }
    Ok(Indexes::Rows(out))
}

/// The indexes of a group key column, as [`indexes`] makes them.
enum Indexes {
    /// The index of each dictionary value and the codes of the rows.
    Coded(*const u16, *const u32),
    /// The index of each row.
    Rows(Vec<u16>),
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

    fn stops_early(&self) -> bool {
        true
    }

    fn at(&self, morsel: &Cut, local: &mut Self::Local) -> Result<()> {
        if let Some(w) = local {
            w.morsel = morsel.index();
        }
        Ok(())
    }

    fn keeps_morsels(&self) -> bool {
        self.0.ordered
    }

    fn sink(&self, chunk: &Chunk, local: &mut Self::Local) -> Result<Progress> {
        match local {
            Some(w) => {
                let (rt, state, out, headers) =
                    (&mut w.rt, &mut w.state, &mut w.out, &mut w.headers);
                let progress = self.0.run(chunk, rt, state, out, &mut false, headers);
                w.from.resize(w.out.len(), w.morsel);
                progress
            }
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

/// About how many pairs of a distinct set a part of the merge gathers.
const PART_PAIRS: usize = 1 << 14;

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

#[cfg(test)]
mod tests {
    use rudb_common::Value;

    use super::*;

    /// The headers of a column read as codes, dereferenced back to bytes, one per row.
    fn read(held: &Held<'_>, rows: usize) -> Vec<Option<Vec<u8>>> {
        let headers = held.values.cast::<u128>();
        (0..rows)
            .map(|i| {
                (held.valid[i / 8] & (1 << (i % 8)) != 0).then(|| {
                    // SAFETY: `codes` is `rows` codes, each below the count of headers at `values`,
                    // whose long strings the test keeps alive.
                    let h =
                        unsafe { headers.add(held.codes.add(i).read() as usize).read_unaligned() };
                    let len = h as u32 as usize;
                    if len <= text::INLINE {
                        h.to_le_bytes()[4..4 + len].to_vec()
                    } else {
                        let at = (h >> 64) as usize as *const u8;
                        // SAFETY: a long header points at `len` bytes of the dictionary's values.
                        unsafe { std::slice::from_raw_parts(at, len) }.to_vec()
                    }
                })
            })
            .collect()
    }

    #[test]
    fn a_text_dictionary_is_read_as_its_values_through_headers_made_once() {
        let words = ["R", "", "a string that is longer than twelve bytes"];
        let values: Vec<Value> = words.iter().map(|w| Value::Varchar((*w).to_string())).collect();
        let dictionary = Arc::new(Vector::from_values(LogicalType::Varchar, &values).unwrap());
        let codes = vec![2, 0, 1, 2, 2, 0, 1, 0, 2];
        let rows = codes.len();
        let v = Vector::stable_dictionary(codes.clone(), Arc::clone(&dictionary)).unwrap();
        let mut known = None;
        assert!(coded(&v, rows, known.as_ref()));
        let first = read(&Held::coded(&v, rows, &mut known).unwrap(), rows);
        let made = known.as_ref().map(|(_, values, _)| Arc::as_ptr(values));
        let again = read(&Held::coded(&v, rows, &mut known).unwrap(), rows);
        assert_eq!(made, known.as_ref().map(|(_, values, _)| Arc::as_ptr(values)));
        let want: Vec<_> =
            codes.iter().map(|&c| Some(words[c as usize].as_bytes().to_vec())).collect();
        assert_eq!(first, want);
        assert_eq!(again, want);
    }
    #[test]
    fn a_like_over_a_coded_column_answers_each_dictionary_value_once() {
        let words = ["http://google.com/", "", "no", "a long string with Google and google in it"];
        let values: Vec<Value> = words.iter().map(|w| Value::Varchar((*w).to_string())).collect();
        let dictionary = Arc::new(Vector::from_values(LogicalType::Varchar, &values).unwrap());
        let codes = vec![3, 0, 1, 2, 3, 3, 0, 2, 1];
        let rows = codes.len();
        let v = Vector::stable_dictionary(codes.clone(), Arc::clone(&dictionary)).unwrap();
        let mut rt = Rt::new(Cancel::new());
        for (pattern, fold) in
            [("%google%", false), ("%GOOGLE%", true), ("http%", false), ("", false)]
        {
            let like = rt.add_like(pattern, fold);
            let mut known = None;
            assert!(worded(&v, rows, known.as_ref()));
            let read = |at: *const u8| -> Vec<u8> {
                let answer = |c: u32| {
                    // SAFETY: every code is below the count of answers.
                    unsafe { at.add(c as usize).read() }
                };
                codes.iter().map(|&c| answer(c)).collect()
            };
            let first = read(like_coded(&rt, like, &v, &mut known).unwrap());
            let made = known.as_ref().map(|(_, answers)| answers.as_ptr());
            let again = read(like_coded(&rt, like, &v, &mut known).unwrap());
            assert_eq!(made, known.as_ref().map(|(_, answers)| answers.as_ptr()));
            let matcher = rt.like(like).unwrap();
            let want: Vec<u8> = codes
                .iter()
                .map(|&c| u8::from(matcher.matches(words[c as usize].as_bytes())))
                .collect();
            assert_eq!(first, want, "{pattern}");
            assert_eq!(again, want, "{pattern}");
        }
    }
}
