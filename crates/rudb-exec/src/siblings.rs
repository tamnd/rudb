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
}

#[derive(Debug)]
pub(crate) struct Walking {
    scratch: Vec<Scratch>,
    /// The keys of a chunk, when they read as one block of integers.
    keys: Vec<i64>,
    /// The siblings of one batch, each once, in the order they were found.
    rids: Vec<u32>,
    /// For each pair of the batch, where its sibling is in `rids` and which row it belongs to.
    places: Vec<u32>,
    owners: Vec<u32>,
    /// For a compared walk, each row of the batch with where its siblings start in `rids` and how
    /// many there are, which is what the pairs come down to when none is made.
    spans: Vec<(u32, u32, u32)>,
    /// The row's side of the comparison, `None` for a null.
    sides: Vec<Option<i64>>,
    /// The sibling's side at each of `rids`, `None` for one that failed a condition or is null.
    values: Vec<Option<i64>>,
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
            Some((at, op, column, other)) => {
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
                let column =
                    walk.read.iter().position(|&(read, ..)| read == column).ok_or_else(missing)?;
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
            key,
            tests,
            starts: OnceLock::new(),
            compaction,
            cancel,
        })
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
        };
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
        let keys = chunk.column(self.key)?;
        local.keys.clear();
        let block = !keys.validity().has_nulls(rows)
            && keys.signed_block(&mut local.keys)
            && local.keys.len() >= rows;
        self.clear(local);
        // The parent of the row before and where its siblings are in the batch, so that a run of
        // rows with one parent looks it up and finds its siblings once.
        let paired = matches!(self.tests, Tests::Pairs { .. });
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
                    local.found.clear();
                    self.children.of(parent, &mut local.cursor, &mut local.found)?;
                    let len = local.found.len();
                    if local.rids.len() + len > VECTOR_SIZE
                        || (paired && local.places.len() + len > VECTOR_SIZE)
                    {
                        self.flush(chunk, local)?;
                        self.clear(local);
                    }
                    // Under a vector's worth, by the test just above.
                    let start = local.rids.len() as u32;
                    for &child in &local.found {
                        let child = u32::try_from(child).map_err(|_| {
                            Error::internal("a sibling row id is past what a gather can hold")
                        })?;
                        if local.rids.last().is_some_and(|&before| before >= child) {
                            local.rising = false;
                        }
                        local.rids.push(child);
                    }
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

    fn flush(&self, chunk: &Chunk, local: &mut Walking) -> Result<()> {
        match &self.tests {
            Tests::Pairs { carried, tests } => self.pairs(chunk, carried, tests, local),
            Tests::Compared { own, op, column, .. } => self.compare(own, *op, *column, local),
        }
    }

    fn clear(&self, local: &mut Walking) {
        local.rids.clear();
        local.places.clear();
        local.owners.clear();
        local.spans.clear();
        local.rising = true;
    }

    /// The stored columns at `rids`, which rise, one vector per column.
    ///
    /// A part the batch reads densely is read whole and kept in `kept`, because the next batch's
    /// siblings are mostly in the same part and a compressed page costs the same to decode for one
    /// row as for all of them. A part read sparsely, which is a child in no order, is read at the
    /// rows asked for.
    fn read(&self, rids: &[u32], kept: &mut Option<(usize, Chunk)>) -> Result<Vec<Vector>> {
        let starts = self.starts()?;
        let mut pieces: Vec<Vec<Vector>> = vec![Vec::new(); self.read.len()];
        let mut positions = Vec::new();
        let mut at = 0;
        while at < rids.len() {
            let rid = u64::from(rids[at]);
            let part = starts.partition_point(|&start| start <= rid).saturating_sub(1);
            let end = *starts
                .get(part + 1)
                .ok_or_else(|| Error::internal("a sibling row id is past the end of its table"))?;
            positions.clear();
            while at < rids.len() && u64::from(rids[at]) < end {
                // Inside the part, whose length is a `usize` it was read into.
                positions.push((u64::from(rids[at]) - starts[part]) as u32);
                at += 1;
            }
            let length = end - starts[part];
            let dense = positions.len() as u64 * DENSE >= length;
            if dense && kept.as_ref().is_none_or(|(at, _)| *at != part) {
                *kept = Some((part, self.rows.read(part, &self.read)?));
            }
            match kept {
                Some((at, whole)) if *at == part => {
                    for (column, piece) in pieces.iter_mut().enumerate() {
                        piece.push(whole.column(column)?.gather(&positions)?);
                    }
                }
                _ => {
                    let read = self.rows.read_rows(part, &self.read, &positions)?;
                    for (column, piece) in pieces.iter_mut().enumerate() {
                        piece.push(read.column(column)?.clone());
                    }
                }
            }
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
        let read = if local.rising {
            self.read(&local.rids, &mut local.part)?
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
            self.read(&local.sorted, &mut local.part)?
        };
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
        let rids = if local.rising {
            &local.rids
        } else {
            local.sorted.clear();
            local.sorted.extend_from_slice(&local.rids);
            local.sorted.sort_unstable();
            local.sorted.dedup();
            local.moved.clear();
            for rid in &local.rids {
                // Every id is in `sorted`, which is no longer than a vector.
                local.moved.push(local.sorted.binary_search(rid).map_or(0, |at| at as u32));
            }
            &local.sorted
        };
        let read = self.read(rids, &mut local.part)?;
        let length = rids.len();
        local.values.clear();
        values_of(
            Chunk::with_rows(read, length)?,
            own,
            column,
            &mut local.scratch,
            &mut local.block,
            &mut local.values,
        )?;
        let holds = |sibling: i64, row: i64| match op {
            CompareOp::Equal => sibling == row,
            CompareOp::NotEqual => sibling != row,
            CompareOp::Less => sibling < row,
            CompareOp::LessOrEqual => sibling <= row,
            CompareOp::Greater => sibling > row,
            CompareOp::GreaterOrEqual => sibling >= row,
            _ => false,
        };
        for &(row, start, len) in &local.spans {
            let Some(side) = local.sides[row as usize] else { continue };
            let found = (start..start + len).any(|place| {
                let at = if local.rising { place } else { local.moved[place as usize] };
                local.values[at as usize].is_some_and(|value| holds(value, side))
            });
            if found {
                local.hit[row as usize] = true;
            }
        }
        Ok(())
    }
}

/// The integer `column` of `chunk` at each row, appended to `out`, `None` for a row that fails one
/// of `own` or is null.
fn values_of(
    chunk: Chunk,
    own: &[Prepared],
    column: usize,
    scratch: &mut [Scratch],
    block: &mut Vec<i64>,
    out: &mut Vec<Option<i64>>,
) -> Result<()> {
    let length = chunk.len();
    let vector = chunk
        .column(column)
        .cloned()
        .map_err(|_| Error::internal("a sibling walk compares a column it did not read"))?;
    let mut sibling = chunk;
    // Which rows are left, as positions in `chunk`, rising.
    let mut left: Option<Vec<u32>> = None;
    for (test, scratch) in own.iter().zip(scratch) {
        if sibling.is_empty() {
            break;
        }
        let kept = test.evaluate_filter(&sibling, scratch)?;
        if kept.len() != sibling.len() {
            left = Some(match left {
                None => kept.indices().to_vec(),
                Some(before) => kept.iter().map(|at| before[at]).collect(),
            });
            sibling = sibling.select(&kept)?;
        }
    }
    let from = out.len();
    signed(&vector, length, block, out);
    if let Some(left) = left {
        let mut next = left.iter().peekable();
        for (at, value) in out[from..].iter_mut().enumerate() {
            if next.peek().is_some_and(|&&kept| kept as usize == at) {
                next.next();
            } else {
                *value = None;
            }
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
            },
            keys: Vec::new(),
            rids: Vec::new(),
            places: Vec::new(),
            owners: Vec::new(),
            spans: Vec::new(),
            sides: Vec::new(),
            values: Vec::new(),
            moved: Vec::new(),
            other: match &self.tests {
                Tests::Pairs { .. } => Scratch::default(),
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
        self.mark(chunk, local)?;
        let wanted = self.kind == JoinKind::Semi;
        let kept = Selection::from_predicate(rows, |row| local.hit[row] == wanted);
        if kept.len() != rows {
            narrow(self.compaction, chunk, &kept, &mut local.gauge)?;
        }
        Ok(Progress::More)
    }

    fn weight(&self) -> usize {
        1
    }
}
