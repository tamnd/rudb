//! A semi or an anti join answered by walking from each row to its siblings.
//!
//! The shape is a join whose other side is a scan of a child table, keyed on the column that
//! table's link was built over, with a condition on top of the equality that the link cannot
//! answer. TPC-H q21 is the one everybody knows: a line of `lineitem` is kept when another line of
//! the same order has a different supplier, and dropped when another line of the same order was
//! late. The hash join gathers one side, reads the other side of `lineitem` again, and matches the
//! two up by order key. But every row that could match is a child of the same parent as the row
//! it is being matched to, and the file already says where those are. So the key goes through the
//! parent's key map to the parent, the parent goes back through the link to its children, and the
//! columns the rest of the condition reads are read at those rows alone. Nothing is gathered, the
//! other side is never scanned, and the rows stream through in the pipeline they arrived in.
//!
//! When one condition alone reads both sides and it compares a column of the sibling with something
//! of the row, which `l2.l_suppkey <> l1.l_suppkey` is, the pairs are never made. The conditions on
//! the sibling alone run once for each sibling rather than once for each pair, and each row compares
//! its own value with the values of its parent's siblings that passed them.
//!
//! A walk straight over another one to the same siblings, which q21's `EXISTS` over its `NOT EXISTS`
//! is, runs as one. Each row's parent is looked up once and its children found once, the columns
//! either walk reads are read once, and each comparison runs over them in turn.
//!
//! It only holds when every child with a key found its parent. A child whose key names no parent
//! is a row the walk cannot reach, so a link with even one of those is refused when the operator is
//! built rather than being a match that goes missing.

use std::sync::{Arc, OnceLock};

use rudb_catalog::Rows;
use rudb_common::{Cancel, Error, Field, LogicalType, Result, Session};
use rudb_graph::{Adjacency, Cursor, KeyMap, Link, NO_PARENT, Rid};
use rudb_pipeline::{Compaction, Gauge, Progress, Stream, narrow};
use rudb_plan::{ColumnBinding, CompareOp, Expr, ExprRef, JoinKind, Plan};
use rudb_seam::{Context, SeamId, Settings};
use rudb_vector::{Chunk, Selection, VECTOR_SIZE, Vector};

use crate::prepared::{Prepared, Scratch};
use crate::register::compaction;
use crate::schema::Schema;

/// A part is read whole when a batch wants at least one of every this many of its rows.
const DENSE: u64 = 64;

/// How a parent's children are found.
#[derive(Debug)]
pub(crate) enum Children {
    /// A child stored in its parent's order, whose children are one run of rows.
    Runs(Arc<Link>),
    /// Any other order, with the children of each parent listed.
    Listed(Arc<Adjacency>),
}

impl Children {
    fn of(&self, parent: Rid, cursor: &mut Cursor, out: &mut Vec<Rid>) -> Result<()> {
        match self {
            Self::Runs(link) => {
                let run = link
                    .backward_from(parent, cursor)
                    .ok_or_else(|| Error::internal("a link has no run for a parent it holds"))?;
                out.extend(run);
                Ok(())
            }
            Self::Listed(adjacency) => adjacency.children_of(parent, out),
        }
    }

    /// Whether `parent` has a child at all, without reading one.
    fn any(&self, parent: Rid, cursor: &mut Cursor) -> Result<bool> {
        match self {
            Self::Runs(link) => {
                let run = link
                    .backward_from(parent, cursor)
                    .ok_or_else(|| Error::internal("a link has no run for a parent it holds"))?;
                Ok(!run.is_empty())
            }
            Self::Listed(adjacency) => Ok(adjacency.degree(parent)? > 0),
        }
    }
}

/// What the builder found, handed over whole.
#[derive(Debug)]
pub(crate) struct Walk {
    pub(crate) kind: JoinKind,
    /// The parent's key map, over the column the link was built against.
    pub(crate) keys: Arc<KeyMap>,
    pub(crate) children: Children,
    /// The child table's rows, which a row id is a position in.
    pub(crate) rows: Rows,
    /// The binding of the scan the join would have read.
    pub(crate) index: u32,
    /// The columns of the child table the conditions read, as the scan's column, the stored
    /// column, and its field.
    pub(crate) read: Vec<(u32, usize, Field)>,
    /// The row's key, a binding on the side that streams through.
    pub(crate) key: ColumnBinding,
    /// Every condition of the join and every filter over the scan, each of which a pair has to pass.
    pub(crate) tests: Vec<ExprRef>,
}

impl Walk {
    /// Whether `other` walks to the same siblings as this from the same rows, and both compare a
    /// column of the sibling with the row, so that the two run as one, see [`Siblings::also`].
    pub(crate) fn fuses_with(&self, plan: &Plan, other: &Self) -> bool {
        let same = match (&self.children, &other.children) {
            (Children::Runs(one), Children::Runs(two)) => Arc::ptr_eq(one, two),
            (Children::Listed(one), Children::Listed(two)) => Arc::ptr_eq(one, two),
            _ => false,
        };
        same && self.key == other.key
            && Arc::ptr_eq(&self.keys, &other.keys)
            && [self, other].iter().all(|walk| {
                !walk.tests.is_empty() && compared(plan, walk).is_some()
            })
    }
}

#[derive(Debug)]
pub(crate) struct Siblings {
    kind: JoinKind,
    keys: Arc<KeyMap>,
    children: Children,
    rows: Rows,
    /// The stored columns read at the siblings, in the order they sit in a pair.
    read: Vec<usize>,
    /// Where the row's key is in the chunks that arrive.
    key: usize,
    tests: Tests,
    /// How many of `read` the first walk reads, which are the first of them.
    width: usize,
    /// A second walk over the same siblings, run in the same pass.
    also: Option<Also>,
    /// The first row of each part and then the row count, worked out on first use.
    starts: OnceLock<Vec<u64>>,
    compaction: &'static dyn Compaction,
    cancel: Cancel,
}

/// How the conditions are run.
#[derive(Debug)]
enum Tests {
    /// Over pairs, each a row beside one of its siblings.
    Pairs {
        /// The columns of an arriving chunk the tests read, in the order they sit in a pair.
        carried: Vec<usize>,
        tests: Vec<Prepared>,
    },
    /// The sibling's `column` against `other`, a value of the row, after the conditions on the
    /// sibling alone.
    Compared {
        own: Vec<Prepared>,
        /// Which way round, with the sibling's column on the left.
        op: CompareOp,
        /// Where the column is among the columns read.
        column: usize,
        other: Box<Prepared>,
    },
    /// None, because the equality was the whole join and the child side was its table read whole.
    /// A row is then kept or dropped on whether its parent has a child at all, which the link or
    /// the adjacency says without a child row being read.
    Bare,
}

/// A second compared walk run over the siblings the first one found and read.
///
/// TPC-H q21 keeps a line when another line of its order has a different supplier and drops it
/// when another line of its order with a different supplier was late. Run one after the other, the
/// two walks looked up the same order, found the same lines and read the same supplier column at
/// them, twice over.
#[derive(Debug)]
struct Also {
    kind: JoinKind,
    own: Vec<Prepared>,
    op: CompareOp,
    /// Where the column is among the columns this walk reads.
    column: usize,
    other: Box<Prepared>,
    /// Where each column this walk reads is among the columns read, in the order this walk reads
    /// them.
    picks: Vec<usize>,
}

#[derive(Debug)]
pub(crate) struct Walking {
    scratch: Vec<Scratch>,
    /// The keys of a chunk, when they read as one block of integers.
    keys: Vec<i64>,
    /// The siblings of one batch, each once, in the order they were found.
    rids: Vec<u32>,
    /// The same siblings as runs of rows, a first row and a length, which is how a walk over a
    /// link finds them and how they are read.
    runs: Vec<(u64, u32)>,
    /// How many siblings the batch has.
    count: usize,
    /// The runs of one part, from its first row.
    within: Vec<(u32, u32)>,
    /// For each pair of the batch, where its sibling is in `rids` and which row it belongs to.
    places: Vec<u32>,
    owners: Vec<u32>,
    /// For a compared walk, each row of the batch with where its siblings start in `rids` and how
    /// many there are, which is what the pairs come down to when none is made.
    spans: Vec<(u32, u32, u32)>,
    /// The row's side of the comparison, `None` for a null.
    sides: Vec<Option<i64>>,
    /// The scratch, the row's side and the rows marked of the second walk, see [`Also`].
    also_scratch: Vec<Scratch>,
    also_other: Scratch,
    also_sides: Vec<Option<i64>>,
    also_hit: Vec<bool>,
    /// The sibling's side at each of `rids`, whatever it is where `pass` is false.
    values: Vec<i64>,
    /// Whether the sibling at each of `rids` passed every condition on it and is not null.
    pass: Vec<bool>,
    /// Where each of `rids` went when they were read in order.
    moved: Vec<u32>,
    other: Scratch,
    /// Whether `rids` rises, which is a child read in its parent's order.
    rising: bool,
    found: Vec<Rid>,
    sorted: Vec<u32>,
    hit: Vec<bool>,
    cursor: Cursor,
    /// The last part read whole, by number, which the next batch's siblings are mostly in.
    part: Option<(usize, Chunk)>,
    block: Vec<i64>,
    gauge: Gauge,
}

impl Siblings {
    /// # Errors
    ///
    /// If a condition does not read as an expression over the pair, or if the key or a column a
    /// condition reads is not in the rows that arrive.
    pub(crate) fn new(
        plan: &Plan,
        walk: Walk,
        arriving: &Schema,
        seams: &Settings,
        cancel: Cancel,
    ) -> Result<Self> {
        let missing = || Error::internal("a sibling walk reads a column its rows do not have");
        let key = arriving.position_of(walk.key).ok_or_else(missing)?;
        let tests = match compared(plan, &walk) {
            _ if walk.tests.is_empty() => Tests::Bare,
            Some((at, op, column, other)) => {
                let (own, column) = sibling_tests(plan, &walk, at, column)?;
                Tests::Compared {
                    own,
                    op,
                    column,
                    other: Box::new(Prepared::one(plan, other, arriving)?),
                }
            }
            None => {
                // The columns of the arriving side the tests read, and none of the others, because
                // every one of them is copied once per pair.
                let mut carried: Vec<usize> = Vec::new();
                for &test in &walk.tests {
                    let mut failed = false;
                    plan.read_columns(test, &mut |_, binding| {
                        if binding.table == walk.index {
                            return;
                        }
                        match arriving.position_of(binding) {
                            Some(at) if !carried.contains(&at) => carried.push(at),
                            Some(_) => {}
                            None => failed = true,
                        }
                    });
                    if failed {
                        return Err(missing());
                    }
                }
                let mut fields = Vec::with_capacity(carried.len() + walk.read.len());
                let mut bindings = Vec::with_capacity(fields.capacity());
                for &at in &carried {
                    fields.push(arriving.fields()[at].clone());
                    bindings.push(arriving.bindings()[at]);
                }
                for (column, _, field) in &walk.read {
                    fields.push(field.clone());
                    bindings.push(ColumnBinding::new(walk.index, *column));
                }
                let pair = Schema::new(fields, bindings)?;
                let tests = walk
                    .tests
                    .iter()
                    .map(|&test| Prepared::one(plan, test, &pair))
                    .collect::<Result<Vec<_>>>()?;
                Tests::Pairs { carried, tests }
            }
        };
        let types = arriving.types();
        let context = Context::new(SeamId::ChunkCompaction, seams).with_types(&types);
        let compaction = compaction().choose(&context)?.strategy();
        Ok(Self {
            kind: walk.kind,
            keys: walk.keys,
            children: walk.children,
            rows: walk.rows,
            read: walk.read.iter().map(|&(_, stored, _)| stored).collect(),
            width: walk.read.len(),
            also: None,
            key,
            tests,
            starts: OnceLock::new(),
            compaction,
            cancel,
        })
    }

    /// This walk with `walk` run in the same pass, over the same siblings, which
    /// [`Walk::fuses_with`] says.
    ///
    /// # Errors
    ///
    /// If either walk does not compare a column of the sibling with the row, or a condition does
    /// not read as an expression over the sibling.
    pub(crate) fn also(mut self, plan: &Plan, walk: Walk, arriving: &Schema) -> Result<Self> {
        let refused = || Error::internal("a sibling walk fused with one that compares nothing");
        if !matches!(self.tests, Tests::Compared { .. }) || walk.tests.is_empty() {
            return Err(refused());
        }
        let (at, op, column, other) = compared(plan, &walk).ok_or_else(refused)?;
        let (own, column) = sibling_tests(plan, &walk, at, column)?;
        let mut picks = Vec::with_capacity(walk.read.len());
        for &(_, stored, _) in &walk.read {
            match self.read.iter().position(|&read| read == stored) {
                Some(at) => picks.push(at),
                None => {
                    picks.push(self.read.len());
                    self.read.push(stored);
                }
            }
        }
        self.also = Some(Also {
            kind: walk.kind,
            own,
            op,
            column,
            other: Box::new(Prepared::one(plan, other, arriving)?),
            picks,
        });
        Ok(self)
    }

    #[must_use]
    pub(crate) fn in_session(mut self, session: &Session) -> Self {
        let all = |tests: Vec<Prepared>| tests.into_iter().map(|test| test.in_session(session));
        self.tests = match self.tests {
            Tests::Pairs { carried, tests } => {
                Tests::Pairs { carried, tests: all(tests).collect() }
            }
            Tests::Compared { own, op, column, other } => Tests::Compared {
                own: all(own).collect(),
                op,
                column,
                other: Box::new(other.in_session(session)),
            },
            Tests::Bare => Tests::Bare,
        };
        self.also = self.also.map(|also| Also {
            own: all(also.own).collect(),
            other: Box::new(also.other.in_session(session)),
            ..also
        });
        self
    }

    fn starts(&self) -> Result<&[u64]> {
        if let Some(starts) = self.starts.get() {
            return Ok(starts);
        }
        let parts = self.rows.chunk_count();
        let mut starts = Vec::with_capacity(parts + 1);
        let mut at = 0_u64;
        starts.push(at);
        for part in 0..parts {
            at += self.rows.chunk_len(part)? as u64;
            starts.push(at);
        }
        Ok(self.starts.get_or_init(|| starts))
    }

    /// Marks the rows of `chunk` with at least one sibling that passes every test.
    ///
    /// The siblings of a parent are found once for a run of rows with that parent, which is a
    /// child read in its own order, where the rows of one parent sit together. The pairs go a
    /// vector's worth at a time.
    fn mark(&self, chunk: &Chunk, local: &mut Walking) -> Result<()> {
        let rows = chunk.len();
        local.hit.clear();
        local.hit.resize(rows, false);
        local.also_hit.clear();
        if self.also.is_some() {
            local.also_hit.resize(rows, false);
        }
        let keys = chunk.column(self.key)?;
        local.keys.clear();
        let block = !keys.validity().has_nulls(rows)
            && keys.signed_block(&mut local.keys)
            && local.keys.len() >= rows;
        if matches!(self.tests, Tests::Bare) {
            return self.parented(keys, block, local);
        }
        self.clear(local);
        // The parent of the row before and where its siblings are in the batch, so that a run of
        // rows with one parent looks it up and finds its siblings once.
        let paired = matches!(self.tests, Tests::Pairs { .. });
        // A walk over a link takes each parent's children as the run they are and never lists
        // them, see [`Self::read`].
        let link = match &self.children {
            Children::Runs(link) => Some(link),
            Children::Listed(_) => None,
        };
        let mut last: Option<(i128, Rid)> = None;
        let mut group: Option<(Rid, u32, u32)> = None;
        for row in 0..rows {
            // A null key matches nothing, and neither does a key the parent does not hold, since
            // every child with a key has the parent that key names.
            let key = if block { Some(i128::from(local.keys[row])) } else { keys.signed_at(row) };
            let Some(key) = key else { continue };
            let parent = match last {
                Some((held, parent)) if held == key => parent,
                _ => {
                    let parent = self.keys.lookup(key)?.unwrap_or(NO_PARENT);
                    last = Some((key, parent));
                    parent
                }
            };
            if parent == NO_PARENT {
                continue;
            }
            let (start, len) = match group {
                Some((held, start, len))
                    if held == parent
                        && (!paired || local.places.len() + len as usize <= VECTOR_SIZE) =>
                {
                    (start, len)
                }
                _ => {
                    let run = match link {
                        Some(link) => {
                            link.backward_from(parent, &mut local.cursor).ok_or_else(|| {
                                Error::internal("a link has no run for a parent it holds")
                            })?
                        }
                        None => {
                            local.found.clear();
                            self.children.of(parent, &mut local.cursor, &mut local.found)?;
                            0..local.found.len() as u64
                        }
                    };
                    // Under the table's rows, which a chunk's length is.
                    let len = (run.end - run.start) as usize;
                    if local.count + len > VECTOR_SIZE
                        || (paired && local.places.len() + len > VECTOR_SIZE)
                    {
                        self.flush(chunk, local)?;
                        self.clear(local);
                    }
                    // Under a vector's worth, by the test just above.
                    let start = local.count as u32;
                    if link.is_some() {
                        if local
                            .runs
                            .last()
                            .is_some_and(|&(first, length)| first + u64::from(length) > run.start)
                        {
                            local.rising = false;
                        }
                        if len > 0 {
                            local.runs.push((run.start, len as u32));
                        }
                    } else {
                        for &child in &local.found {
                            let child = u32::try_from(child).map_err(|_| {
                                Error::internal("a sibling row id is past what a gather can hold")
                            })?;
                            if local.rids.last().is_some_and(|&before| before >= child) {
                                local.rising = false;
                            }
                            local.rids.push(child);
                        }
                    }
                    local.count += len;
                    group = Some((parent, start, len as u32));
                    (start, len as u32)
                }
            };
            if paired {
                for place in start..start + len {
                    local.places.push(place);
                    // Under the chunk's length.
                    local.owners.push(row as u32);
                }
            } else {
                // Under the chunk's length.
                local.spans.push((row as u32, start, len));
            }
        }
        if !local.places.is_empty() || !local.spans.is_empty() {
            self.flush(chunk, local)?;
        }
        Ok(())
    }

    /// Marks the rows whose parent has a child, for a walk with nothing to test. A run of rows
    /// with one key looks its parent up once.
    fn parented(&self, keys: &Vector, block: bool, local: &mut Walking) -> Result<()> {
        let mut last: Option<(i128, bool)> = None;
        for row in 0..local.hit.len() {
            // A null key matches nothing, and neither does a key the parent does not hold, since
            // every child with a key has the parent that key names.
            let key = if block { Some(i128::from(local.keys[row])) } else { keys.signed_at(row) };
            let Some(key) = key else { continue };
            let found = match last {
                Some((held, found)) if held == key => found,
                _ => {
                    let parent = self.keys.lookup(key)?.unwrap_or(NO_PARENT);
                    let found =
                        parent != NO_PARENT && self.children.any(parent, &mut local.cursor)?;
                    last = Some((key, found));
                    found
                }
            };
            local.hit[row] = found;
        }
        Ok(())
    }

    fn flush(&self, chunk: &Chunk, local: &mut Walking) -> Result<()> {
        match &self.tests {
            Tests::Pairs { carried, tests } => self.pairs(chunk, carried, tests, local),
            Tests::Compared { own, op, column, .. } => self.compare(own, *op, *column, local),
            Tests::Bare => Ok(()),
        }
    }

    fn clear(&self, local: &mut Walking) {
        local.rids.clear();
        local.runs.clear();
        local.count = 0;
        local.places.clear();
        local.owners.clear();
        local.spans.clear();
        local.rising = true;
    }

    /// The stored columns at `runs`, which rise and do not overlap, one vector per column.
    ///
    /// A part the batch reads densely is read whole and kept in `kept`, because the next batch's
    /// siblings are mostly in the same part and a compressed page costs the same to decode for one
    /// row as for all of them. A part read sparsely, which is a child in no order, is read at the
    /// rows asked for. `within` is room for the runs of one part.
    fn read(
        &self,
        runs: &[(u64, u32)],
        kept: &mut Option<(usize, Chunk)>,
        within: &mut Vec<(u32, u32)>,
    ) -> Result<Vec<Vector>> {
        let starts = self.starts()?;
        let past = || Error::internal("a sibling row id is past the end of its table");
        let mut pieces: Vec<Vec<Vector>> = vec![Vec::new(); self.read.len()];
        let mut part = 0;
        let mut wanted = 0;
        within.clear();
        for &(first, length) in runs {
            let (mut from, end) = (first, first + u64::from(length));
            while from < end {
                let stop = *starts.get(part + 1).ok_or_else(past)?;
                if from < starts[part] || from >= stop {
                    if !within.is_empty() {
                        self.read_part(part, within, wanted, kept, &mut pieces)?;
                        within.clear();
                        wanted = 0;
                    }
                    part = starts.partition_point(|&start| start <= from).saturating_sub(1);
                    continue;
                }
                let take = (end - from).min(stop - from);
                // Inside the part, whose length is a `usize` it was read into.
                within.push(((from - starts[part]) as u32, take as u32));
                wanted += take;
                from += take;
            }
        }
        if !within.is_empty() {
            self.read_part(part, within, wanted, kept, &mut pieces)?;
        }
        pieces
            .into_iter()
            .map(|mut piece| {
                if piece.len() == 1 {
                    return piece
                        .pop()
                        .ok_or_else(|| Error::internal("a sibling read lost its piece"));
                }
                let ty = piece[0].logical_type().clone();
                rudb_vector::concat(&ty, &piece)?
                    .ok_or_else(|| Error::internal("the parts of a sibling read did not join"))
            })
            .collect()
    }

    /// The runs of part `part`, `wanted` rows between them, one piece a column.
    fn read_part(
        &self,
        part: usize,
        runs: &[(u32, u32)],
        wanted: u64,
        kept: &mut Option<(usize, Chunk)>,
        pieces: &mut [Vec<Vector>],
    ) -> Result<()> {
        let starts = self.starts()?;
        let length = starts[part + 1] - starts[part];
        if wanted * DENSE >= length && kept.as_ref().is_none_or(|(at, _)| *at != part) {
            *kept = Some((part, self.rows.read(part, &self.read)?));
        }
        match kept {
            Some((at, whole)) if *at == part => {
                for (column, piece) in pieces.iter_mut().enumerate() {
                    piece.push(whole.column(column)?.gather_runs(runs)?);
                }
            }
            _ => {
                let positions: Vec<u32> =
                    runs.iter().flat_map(|&(start, length)| start..start + length).collect();
                let read = self.rows.read_rows(part, &self.read, &positions)?;
                for (column, piece) in pieces.iter_mut().enumerate() {
                    piece.push(read.column(column)?.clone());
                }
            }
        }
        Ok(())
    }

    /// One batch of pairs, each a row of `chunk` at `owners` beside its sibling at `places`.
    fn pairs(
        &self,
        chunk: &Chunk,
        carried: &[usize],
        tests: &[Prepared],
        local: &mut Walking,
    ) -> Result<()> {
        // A child in no order, or a parent that came back after another, is read in row order and
        // each pair's place moved to where its sibling went.
        let listed = matches!(self.children, Children::Listed(_));
        if !local.rising && !listed {
            list(local)?;
        }
        if local.rising {
            if listed {
                coalesce(&local.rids, &mut local.runs);
            }
        } else {
            local.sorted.clear();
            local.sorted.extend_from_slice(&local.rids);
            local.sorted.sort_unstable();
            local.sorted.dedup();
            for place in &mut local.places {
                let rid = local.rids[*place as usize];
                // Every id is in `sorted`, which is no longer than a vector.
                *place = local.sorted.binary_search(&rid).map_or(0, |at| at as u32);
            }
            coalesce(&local.sorted, &mut local.runs);
        }
        let read = self.read(&local.runs, &mut local.part, &mut local.within)?;
        let mut columns = Vec::with_capacity(carried.len() + read.len());
        for &at in carried {
            columns.push(chunk.column(at)?.gather(&local.owners)?);
        }
        for column in read {
            columns.push(column.gather(&local.places)?);
        }
        let mut pair = Chunk::with_rows(columns, local.places.len())?;
        let mut owners = std::mem::take(&mut local.owners);
        for (test, scratch) in tests.iter().zip(&mut local.scratch) {
            if pair.is_empty() {
                break;
            }
            let kept = test.evaluate_filter(&pair, scratch)?;
            if kept.len() != pair.len() {
                owners.retain({
                    let mut at = 0;
                    let mut next = kept.iter().peekable();
                    move |_| {
                        let keep = next.peek() == Some(&at);
                        if keep {
                            next.next();
                        }
                        at += 1;
                        keep
                    }
                });
                pair = pair.select(&kept)?;
            }
        }
        for &owner in &owners {
            local.hit[owner as usize] = true;
        }
        local.owners = owners;
        Ok(())
    }
}

impl Siblings {
    /// One batch of rows, each compared with the siblings at its span that pass the conditions on
    /// the sibling alone.
    fn compare(
        &self,
        own: &[Prepared],
        op: CompareOp,
        column: usize,
        local: &mut Walking,
    ) -> Result<()> {
        // A walk over a link found runs and has no rows listed. A parent that came back after another
        // lists them here and goes the way every walk in no order goes.
        let listed = matches!(self.children, Children::Listed(_));
        if !local.rising && !listed {
            list(local)?;
        }
        if !local.rising {
            local.sorted.clear();
            local.sorted.extend_from_slice(&local.rids);
            local.sorted.sort_unstable();
            local.sorted.dedup();
            local.moved.clear();
            for rid in &local.rids {
                // Every id is in `sorted`, which is no longer than a vector.
                local.moved.push(local.sorted.binary_search(rid).map_or(0, |at| at as u32));
            }
            coalesce(&local.sorted, &mut local.runs);
        } else if listed {
            coalesce(&local.rids, &mut local.runs);
        }
        let mut read = self.read(&local.runs, &mut local.part, &mut local.within)?;
        let length = local.runs.iter().map(|&(_, length)| length as usize).sum();
        let picked = self.also.as_ref().map(|also| {
            also.picks.iter().map(|&at| read[at].clone()).collect::<Vec<_>>()
        });
        read.truncate(self.width);
        values_of(
            Chunk::with_rows(read, length)?,
            own,
            column,
            &mut local.scratch,
            &mut local.values,
            &mut local.pass,
        )?;
        settle(op, local, false);
        if let (Some(also), Some(picked)) = (&self.also, picked) {
            values_of(
                Chunk::with_rows(picked, length)?,
                &also.own,
                also.column,
                &mut local.also_scratch,
                &mut local.values,
                &mut local.pass,
            )?;
            settle(also.op, local, true);
        }
        Ok(())
    }
}

/// Marks the rows of the batch with a sibling that stands in `op` to the row, for the second walk
/// when `second` is set.
///
/// The operator is settled once for the batch, so that the loop over the siblings has no match in
/// it.
fn settle(op: CompareOp, local: &mut Walking, second: bool) {
    match op {
        CompareOp::Equal => found(local, second, |sibling, row| sibling == row),
        CompareOp::NotEqual => found(local, second, |sibling, row| sibling != row),
        CompareOp::Less => found(local, second, |sibling, row| sibling < row),
        CompareOp::LessOrEqual => found(local, second, |sibling, row| sibling <= row),
        CompareOp::Greater => found(local, second, |sibling, row| sibling > row),
        CompareOp::GreaterOrEqual => found(local, second, |sibling, row| sibling >= row),
        _ => {}
    }
}

/// Marks each row of the batch with a sibling whose value `holds` against the row's side, the
/// second walk's side and marks when `second` is set.
fn found(local: &mut Walking, second: bool, holds: impl Fn(i64, i64) -> bool) {
    let Walking { spans, sides, also_sides, hit, also_hit, values, pass, moved, rising, .. } =
        local;
    let (sides, hit) = if second { (also_sides, also_hit) } else { (sides, hit) };
    for &(row, start, len) in spans.iter() {
        let Some(side) = sides[row as usize] else { continue };
        let places = start as usize..(start + len) as usize;
        let found = if *rising {
            values[places.clone()]
                .iter()
                .zip(&pass[places])
                .any(|(&value, &pass)| pass & holds(value, side))
        } else {
            moved[places].iter().any(|&at| {
                let at = at as usize;
                pass[at] & holds(values[at], side)
            })
        };
        if found {
            hit[row as usize] = true;
        }
    }
}

/// The conditions of `walk` on the sibling alone, which are all but the one at `at`, over the
/// columns it reads, and where `column`, the sibling's side of that one, is among them.
fn sibling_tests(
    plan: &Plan,
    walk: &Walk,
    at: usize,
    column: u32,
) -> Result<(Vec<Prepared>, usize)> {
    let mut fields = Vec::with_capacity(walk.read.len());
    let mut bindings = Vec::with_capacity(walk.read.len());
    for (column, _, field) in &walk.read {
        fields.push(field.clone());
        bindings.push(ColumnBinding::new(walk.index, *column));
    }
    let sibling = Schema::new(fields, bindings)?;
    let own = walk
        .tests
        .iter()
        .enumerate()
        .filter(|&(test, _)| test != at)
        .map(|(_, &test)| Prepared::one(plan, test, &sibling))
        .collect::<Result<Vec<_>>>()?;
    let column = walk
        .read
        .iter()
        .position(|&(read, ..)| read == column)
        .ok_or_else(|| Error::internal("a sibling walk compares a column it does not read"))?;
    Ok((own, column))
}

/// The runs a walk over a link found, as the rows of `local`, for a batch whose parents did not
/// rise and which is read the way a walk in no order is.
fn list(local: &mut Walking) -> Result<()> {
    local.rids.clear();
    for &(first, length) in &local.runs {
        for rid in first..first + u64::from(length) {
            local.rids.push(
                u32::try_from(rid).map_err(|_| {
                    Error::internal("a sibling row id is past what a gather can hold")
                })?,
            );
        }
    }
    Ok(())
}

/// `rids`, which rise, as runs of rows, a first row and a length, in `runs`.
fn coalesce(rids: &[u32], runs: &mut Vec<(u64, u32)>) {
    runs.clear();
    for &rid in rids {
        match runs.last_mut() {
            Some((first, length)) if *first + u64::from(*length) == u64::from(rid) => *length += 1,
            _ => runs.push((u64::from(rid), 1)),
        }
    }
}

/// The integer `column` of `chunk` at each row in `values`, and in `pass` whether the row passed
/// every one of `own` and is not null.
///
/// A flag and a value rather than an `Option` a row, and the rows a test keeps marked rather than
/// the ones it drops cleared one at a time. On TPC-H q21 the walk to the late lines of an order has
/// one condition on the sibling, and writing a `None` over each sibling it failed was most of what
/// the batch cost outside the reads. The last test does not cut the chunk down either, because
/// nothing evaluates over what it would keep.
fn values_of(
    chunk: Chunk,
    own: &[Prepared],
    column: usize,
    scratch: &mut [Scratch],
    values: &mut Vec<i64>,
    pass: &mut Vec<bool>,
) -> Result<()> {
    let length = chunk.len();
    let vector = chunk
        .column(column)
        .map_err(|_| Error::internal("a sibling walk compares a column it did not read"))?;
    pass.clear();
    if !vector.validity().has_nulls(length) && vector.signed_block(values) && values.len() >= length
    {
        values.truncate(length);
        pass.resize(length, true);
    } else {
        values.clear();
        for at in 0..length {
            let value = vector.signed_at(at).and_then(|value| i64::try_from(value).ok());
            values.push(value.unwrap_or(0));
            pass.push(value.is_some());
        }
    }
    let mut sibling = chunk;
    // Which rows are left, as positions in `chunk`, rising, once a test has dropped one.
    let mut left: Option<Vec<u32>> = None;
    // The rows of `chunk` the tests so far kept.
    let mut kept_rows: Vec<bool> = Vec::new();
    for (at, (test, scratch)) in own.iter().zip(scratch).enumerate() {
        if sibling.is_empty() {
            break;
        }
        let kept = test.evaluate_filter(&sibling, scratch)?;
        if kept.len() == sibling.len() {
            continue;
        }
        kept_rows.clear();
        kept_rows.resize(length, false);
        match &left {
            None => kept.indices().iter().for_each(|&row| kept_rows[row as usize] = true),
            Some(before) => {
                kept.iter().for_each(|row| kept_rows[before[row] as usize] = true);
            }
        }
        pass.iter_mut().zip(&kept_rows).for_each(|(pass, &kept)| *pass &= kept);
        if at + 1 < own.len() {
            left = Some(match left {
                None => kept.indices().to_vec(),
                Some(before) => kept.iter().map(|row| before[row]).collect(),
            });
            sibling = sibling.select(&kept)?;
        }
    }
    Ok(())
}

/// The first `length` values of an integer `vector` appended to `out`, `None` for a null.
fn signed(vector: &Vector, length: usize, block: &mut Vec<i64>, out: &mut Vec<Option<i64>>) {
    if !vector.validity().has_nulls(length) && vector.signed_block(block) && block.len() >= length {
        out.extend(block[..length].iter().map(|&value| Some(value)));
    } else {
        out.extend(
            (0..length).map(|at| vector.signed_at(at).and_then(|value| i64::try_from(value).ok())),
        );
    }
}

/// The one condition that reads both sides, when it compares a column of the sibling with
/// something of the row and every other condition reads the sibling alone, as the condition's
/// place among the tests, the comparison with the sibling's side on the left, the sibling's
/// column, and the row's side.
///
/// Both sides have to be integers or both dates of the same type, which compare as the integers
/// they are stored as. A comparison that casts one side, or one over decimals of different scales,
/// is not one a pair of stored values can answer and is left to the pairs.
fn compared(plan: &Plan, walk: &Walk) -> Option<(usize, CompareOp, u32, ExprRef)> {
    let mut found = None;
    for (at, &test) in walk.tests.iter().enumerate() {
        let (mut sibling, mut row) = (false, false);
        plan.read_columns(test, &mut |_, binding| {
            if binding.table == walk.index {
                sibling = true;
            } else {
                row = true;
            }
        });
        if !row {
            continue;
        }
        if !sibling || found.is_some() {
            return None;
        }
        found = Some((at, test));
    }
    let (at, test) = found?;
    let &Expr::Compare { op, left, right } = plan.expr(test) else { return None };
    let flipped = match op {
        CompareOp::Equal | CompareOp::NotEqual => op,
        CompareOp::Less => CompareOp::Greater,
        CompareOp::LessOrEqual => CompareOp::GreaterOrEqual,
        CompareOp::Greater => CompareOp::Less,
        CompareOp::GreaterOrEqual => CompareOp::LessOrEqual,
        _ => return None,
    };
    let reads_sibling = |expr: ExprRef| {
        let mut reads = false;
        plan.read_columns(expr, &mut |_, binding| reads |= binding.table == walk.index);
        reads
    };
    let (op, column, other) = match (plan.expr(left), plan.expr(right)) {
        (&Expr::Column(column), _) if column.table == walk.index && !reads_sibling(right) => {
            (op, column, right)
        }
        (_, &Expr::Column(column)) if column.table == walk.index && !reads_sibling(left) => {
            (flipped, column, left)
        }
        _ => return None,
    };
    let ty = plan.expr_type(left);
    let whole = matches!(
        ty,
        LogicalType::TinyInt
            | LogicalType::SmallInt
            | LogicalType::Integer
            | LogicalType::BigInt
            | LogicalType::UTinyInt
            | LogicalType::USmallInt
            | LogicalType::UInteger
            | LogicalType::Date
    );
    (whole && ty == plan.expr_type(right)).then_some((at, op, column.column, other))
}

impl Stream for Siblings {
    type Local = Walking;

    fn local(&self) -> Walking {
        Walking {
            scratch: match &self.tests {
                Tests::Pairs { tests, .. } => tests.iter().map(Prepared::scratch).collect(),
                Tests::Compared { own, .. } => own.iter().map(Prepared::scratch).collect(),
                Tests::Bare => Vec::new(),
            },
            keys: Vec::new(),
            rids: Vec::new(),
            runs: Vec::new(),
            count: 0,
            within: Vec::new(),
            places: Vec::new(),
            owners: Vec::new(),
            spans: Vec::new(),
            sides: Vec::new(),
            also_scratch: self
                .also
                .as_ref()
                .map(|also| also.own.iter().map(Prepared::scratch).collect())
                .unwrap_or_default(),
            also_other: self.also.as_ref().map(|also| also.other.scratch()).unwrap_or_default(),
            also_sides: Vec::new(),
            also_hit: Vec::new(),
            values: Vec::new(),
            pass: Vec::new(),
            moved: Vec::new(),
            other: match &self.tests {
                Tests::Pairs { .. } | Tests::Bare => Scratch::default(),
                Tests::Compared { other, .. } => other.scratch(),
            },
            rising: true,
            found: Vec::new(),
            sorted: Vec::new(),
            hit: Vec::new(),
            cursor: Cursor::default(),
            part: None,
            block: Vec::new(),
            gauge: Gauge::new(1),
        }
    }

    fn push(&self, chunk: &mut Chunk, local: &mut Walking) -> Result<Progress> {
        self.cancel.check()?;
        let rows = chunk.len();
        if rows == 0 {
            return Ok(Progress::More);
        }
        if let Tests::Compared { other, .. } = &self.tests {
            let sides = other.evaluate_one(chunk, &mut local.other)?;
            local.sides.clear();
            signed(sides, rows, &mut local.block, &mut local.sides);
        }
        if let Some(also) = &self.also {
            let sides = also.other.evaluate_one(chunk, &mut local.also_other)?;
            local.also_sides.clear();
            signed(sides, rows, &mut local.block, &mut local.also_sides);
        }
        self.mark(chunk, local)?;
        let wanted = self.kind == JoinKind::Semi;
        let also = self.also.as_ref().map(|also| also.kind == JoinKind::Semi);
        let kept = Selection::from_predicate(rows, |row| {
            local.hit[row] == wanted && also.is_none_or(|wanted| local.also_hit[row] == wanted)
        });
        if kept.len() != rows {
            narrow(self.compaction, chunk, &kept, &mut local.gauge)?;
        }
        Ok(Progress::More)
    }

    fn weight(&self) -> usize {
        1
    }
}
