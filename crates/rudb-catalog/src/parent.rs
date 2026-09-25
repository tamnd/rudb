//! One column of a table held end to end, which is what a link join gathers out of.
//!
//! spec/graph/05-execution.md section 5.2 says the parent columns of a link join come out as
//! `Gathered`, holding an `Arc` to the parent's column vector and the `rid`s to take from it. This
//! is where that vector comes from. A stored table is read a part at a time, so the whole of one
//! column is the parts of it laid end to end, and laying them end to end is
//! [`rudb_vector::concat`], which is the same call a row group of a stored table is already built
//! with.
//!
//! # Why the whole column and not a part at a time
//!
//! Because the `rid`s a child chunk carries are `rid`s of the parent *table*, and two thousand of
//! them out of a clustered child may still straddle a part boundary while two thousand out of an
//! unclustered child straddle the whole table. A gather whose source was one part would have to be
//! several gathers with the rows interleaved back together afterwards, which is an assembly per
//! chunk per column to answer something that is a shift and a load once the column is contiguous.
//!
//! The cost of that decision is stated rather than hidden: this holds the projected columns of the
//! parent in memory for as long as the query runs. That is the same thing a hash join's build side
//! does, minus the hash table, the tuple layout and the copy per matching child row, and it is why
//! section 6.4's rule is about the width of the parent's *projection* rather than about the width
//! of the parent.
//!
//! # A stored column does not arrive flat
//!
//! It arrives in whatever form the writer chose, which on real data is bit packed for an integer
//! and dictionary encoded for a low cardinality string, and almost never flat.
//! [`rudb_vector::concat`] lays flat runs end to end and declines everything else, on the argument
//! that a caller who gets a `None` has somewhere to put the pieces.
//!
//! This caller does not. The `rid` a child row carries names a row of the parent table, and a run
//! that is still in pieces has no row at that offset, so the pieces have to become one run before
//! anything can be taken out of them. So a piece that is not flat is flattened here.
//!
//! The cost of that is one decode of one column, once per query, and it is worth being explicit
//! that it is not new work: the hash join this replaces decodes the same values to build its table
//! over them, and then writes each of them into a tuple as well. What is given up is the encoding's
//! size in memory, which is why the budget below is measured after the flattening rather than
//! before it.
//!
//! An earlier version of this file did not flatten and passed the `None` on. Every link join over
//! every table anybody had written with rudb's own writer then failed, and it failed reporting that
//! it was out of memory, which it was not. The measurement that found it is `cargo xtask sections`.
//!
//! # The budget, and what a refusal means
//!
//! [`Parent::column`] answers `None` rather than an error when a column would not fit, which is a
//! memory limit being reached and not a bug. What the caller does with it is the caller's: the link
//! join operator reports it, on the argument that a parent whose projection will not fit is one
//! whose hash join would not have fit either.
//!
//! The budget is counted over what is held rather than estimated before the read, because a column
//! is compressed in the file and the number that matters is what it costs once it is a vector. A
//! column that turns out not to fit is dropped rather than kept, so the next query is not refused
//! because of a column this one could not use anyway.
//!
//! # By part
//!
//! The whole column is what a link join used to gather from, and it made the join cost what a hash
//! join costs: every value of the parent decoded, and then laid end to end, before the first child
//! row arrived. On TPC-H q12 at scale factor one that was 1.5 million `o_orderpriority` values for
//! 30,988 surviving children. [`Parent::place`] and [`Parent::gather`] are the other way to do it.
//! A part is decoded the first time a chunk's row ids land in it and kept, and a part nothing
//! landed in is never read. A chunk that takes a real share of a part reads it whole, in the form
//! the writer stored it in, and keeps it for the chunks after; a chunk that takes a few rows of a
//! part reads those rows and keeps nothing. A chunk that lands in one part, which is what a
//! child stored in its parent's order does, is one gather out of that part. A chunk that lands in
//! several is one gather per part it reached and then one typed copy per row to put the rows back
//! in the chunk's order, which is [`rudb_vector::picked`].
//!
//! Nothing here is laid end to end and nothing is decoded that no survivor asked for, so the worst
//! case, a child whose survivors reach every part, reads every part and decodes one value per
//! survivor. The whole column read decoded every value of the parent in that case and in every
//! other.
//!

use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, OnceLock};

use rudb_common::{Error, LogicalType, Result, Spread, Value, serially};
use rudb_vector::{Form, NO_ROW, Validity, Vector, concat_on, picked};

use crate::table::Rows;

/// How few of a part's rows a chunk has to take for the part to be read at those rows alone rather
/// than whole: fewer than one in this many.
const SPARSE: usize = 16;

/// One slot per part of one column, each empty until a gather reads that part whole.
type Slots = Arc<[Mutex<Option<Arc<Vector>>>]>;

/// The projected columns of one table, each held whole, each read at most once.
///
/// One of these per parent table per query. It is shared rather than cloned, because the point of
/// the whole exercise is that eight columns gathered off one parent are one parent between them,
/// which is also what [`Vector::footprint`] reports about the vectors that come out of it.
#[derive(Debug)]
pub struct Parent {
    rows: Rows,
    /// The columns already read, by column index. A `None` is a column that was asked for and did
    /// not fit, remembered so that a second ask does not read it again to refuse it again.
    held: Mutex<HashMap<usize, Option<Arc<Vector>>>>,
    /// What every column held here may cost together, in bytes.
    budget: usize,
    /// The first row of each part and then the row count, with the part of every stretch of row
    /// ids, worked out on first use.
    starts: OnceLock<Directory>,
    /// The parts read so far for [`Self::gather`], by column and then by part, in stored form.
    parts: Mutex<HashMap<usize, Slots>>,
    /// What the parts held in `parts` cost between them.
    spent_parts: AtomicUsize,
    /// The parts, by column and then by part, that a chunk has read a few rows of already.
    visited: Mutex<HashSet<(usize, usize)>>,
    /// Set once a column read whole for a scattered chunk went past the budget, so that later
    /// chunks go by part without trying again.
    refused: AtomicBool,
}

/// Where the parent rows one chunk asked for are, worked out once and used for every column.
#[derive(Debug)]
pub struct Placement {
    rows: usize,
    shape: Shape,
}

#[derive(Debug)]
enum Shape {
    /// No row has a parent.
    Nowhere,
    /// Every row with a parent has it in this part, at these offsets into it.
    One(usize, Arc<Vec<u32>>),
    /// Several parts, each with the offsets of the rows that landed in it in the order they landed,
    /// and for each row which of those parts and which of its offsets, with [`NO_ROW`] as the part
    /// of a row that has no parent.
    Many(Vec<(usize, Vec<u32>)>, Vec<(u32, u32)>),
    /// More than half the parts, kept as the row ids themselves and gathered out of the column read
    /// whole, with the number of parts reached.
    Whole(usize, Vec<u32>),
}

impl Placement {
    /// How many parts the chunk landed in.
    #[must_use]
    pub fn parts(&self) -> usize {
        match &self.shape {
            Shape::Nowhere => 0,
            Shape::One(..) => 1,
            Shape::Many(parts, _) => parts.len(),
            Shape::Whole(parts, _) => *parts,
        }
    }
}

/// Which part a row id is in, answered with a load rather than a search.
///
/// The row ids are cut into stretches of `1 << shift`, no longer than the shortest part but the
/// last, so a stretch starts in some part and ends in that one or the next. `first` holds the part
/// each stretch starts in, and a row id is its stretch's part or a step or two past it. The search
/// this replaces was most of what placing a chunk cost when its rows jump between parts, which is
/// a child stored in another order than its parent's.
#[derive(Debug)]
struct Directory {
    /// The first row of each part and then the row count.
    starts: Vec<u64>,
    shift: u32,
    first: Vec<u32>,
}

/// The most stretches a directory holds, so that a table of many short parts costs a search's
/// worth of steps rather than a directory as long as the table.
const STRETCHES: u64 = 1 << 16;

impl Directory {
    fn new(starts: Vec<u64>) -> Self {
        let total = starts.last().copied().unwrap_or(0);
        let parts = starts.len().saturating_sub(1);
        let shortest = starts
            .windows(2)
            .take(parts.saturating_sub(1))
            .map(|pair| pair[1] - pair[0])
            .filter(|&len| len > 0)
            .min()
            .unwrap_or(total.max(1));
        let mut shift = 63 - shortest.max(1).leading_zeros();
        while (total >> shift) >= STRETCHES {
            shift += 1;
        }
        let first = (0..=(total >> shift))
            .map(|stretch| {
                // Under the part count, which is far under a `u32`.
                (starts.partition_point(|&start| start <= stretch << shift).saturating_sub(1))
                    .min(parts.saturating_sub(1)) as u32
            })
            .collect();
        Self { starts, shift, first }
    }

    /// The part row `at` is in. `at` has to be under the row count.
    fn part_of(&self, at: u64) -> usize {
        let mut part = self.first[(at >> self.shift) as usize] as usize;
        while at >= self.starts[part + 1] {
            part += 1;
        }
        part
    }
}

impl Parent {
    /// A parent whose columns may cost `budget` bytes between them.
    #[must_use]
    pub fn new(rows: Rows, budget: usize) -> Self {
        Self {
            rows,
            held: Mutex::new(HashMap::new()),
            budget,
            starts: OnceLock::new(),
            parts: Mutex::new(HashMap::new()),
            spent_parts: AtomicUsize::new(0),
            visited: Mutex::new(HashSet::new()),
            refused: AtomicBool::new(false),
        }
    }

    /// Where the parent rows `rids` name are, with [`NO_ROW`] for a row that has no parent.
    ///
    /// A chunk that reaches more than half the parts of a parent of several is placed whole: the
    /// gather reads the column once, end to end, and indexes it by row id. That is a child stored
    /// in another order than its parent's, `lineitem` against `partsupp` on TPC-H q09, where every
    /// chunk reached every part and paid for each row a search for its part, a lookup of that part
    /// in a map, a gather per part and a pick to put the rows back in order. The parts it would have
    /// read are the same ones.
    ///
    /// # Errors
    ///
    /// If a part cannot say how many rows it has, or if a row id is past the end of the table.
    pub fn place(&self, rids: &[u32]) -> Result<Placement> {
        let directory = self.starts()?;
        let starts = directory.starts.as_slice();
        let total = starts.last().copied().unwrap_or(0);
        let parts = starts.len().saturating_sub(1);
        // For each part, its place among the parts this chunk reached, in the order reached.
        let mut numbered = vec![NO_ROW; parts];
        let mut reached: Vec<usize> = Vec::new();
        // The parts first and nothing else, because a chunk placed whole needs only how many it
        // reached, and on TPC-H q09 every chunk against `partsupp` is placed whole. The part of the
        // row before is tried first, because a child stored in its parent's order asks for the same
        // part a couple of thousand times in a row.
        let mut last = 0;
        for &rid in rids {
            if rid == NO_ROW {
                continue;
            }
            let at = u64::from(rid);
            if at >= total {
                return Err(Error::internal(format!(
                    "a gathered row id is past the {total} rows of its parent"
                )));
            }
            if !(starts[last] <= at && at < starts[last + 1]) {
                last = directory.part_of(at);
            }
            if numbered[last] == NO_ROW {
                // Under the part count, which is far under a `u32`.
                numbered[last] = reached.len() as u32;
                reached.push(last);
            }
        }
        if reached.len() > 1 && reached.len() * 2 > parts && !self.refused.load(Ordering::Relaxed) {
            return Ok(Placement {
                rows: rids.len(),
                shape: Shape::Whole(reached.len(), rids.to_vec()),
            });
        }
        // Each row's part and offset in it, with [`NO_ROW`] as the part of a row with no parent.
        let mut found: Vec<(u32, u32)> = Vec::with_capacity(rids.len());
        let mut last = 0;
        for &rid in rids {
            if rid == NO_ROW {
                found.push((NO_ROW, 0));
                continue;
            }
            let at = u64::from(rid);
            if !(starts[last] <= at && at < starts[last + 1]) {
                last = directory.part_of(at);
            }
            // In a part, so under its row count, which is a `usize` the part was read into.
            found.push((numbered[last], (at - starts[last]) as u32));
        }
        let shape = match reached.as_slice() {
            [] => Shape::Nowhere,
            &[only] => Shape::One(
                only,
                Arc::new(
                    found
                        .iter()
                        .map(|&(part, row)| if part == NO_ROW { NO_ROW } else { row })
                        .collect(),
                ),
            ),
            _ => {
                // Numbered in the order they are first reached. A child walking its parent forwards
                // reaches them in part order, and nothing below depends on it either way.
                let mut offsets: Vec<(usize, Vec<u32>)> =
                    reached.iter().map(|&part| (part, Vec::new())).collect();
                let picks = found
                    .iter()
                    .map(|&(at, row)| {
                        if at == NO_ROW {
                            return (NO_ROW, 0);
                        }
                        let held = &mut offsets[at as usize].1;
                        held.push(row);
                        // Under the chunk's length, since a part gets no more rows than it has.
                        (at, (held.len() - 1) as u32)
                    })
                    .collect();
                Shape::Many(offsets, picks)
            }
        };
        Ok(Placement { rows: rids.len(), shape })
    }

    /// One column of the parent at the rows `placement` holds, or `None` if the parts it needs
    /// would go past the budget.
    ///
    /// # Errors
    ///
    /// If a part cannot be read, or if the pieces of a chunk that landed in several parts do not
    /// lay end to end.
    pub fn gather(
        &self,
        column: usize,
        ty: &LogicalType,
        placement: &Placement,
    ) -> Result<Option<Vector>> {
        match &placement.shape {
            Shape::Nowhere => Ok(Some(Vector::constant(ty.clone(), Value::Null, placement.rows))),
            Shape::One(part, offsets) => self.rows_in(column, *part, offsets),
            Shape::Whole(_, rids) => {
                if let Some(whole) = self.column(column, ty)? {
                    return whole.gather(rids).map(Some);
                }
                // Past the budget, so this chunk and every one after it go by part.
                self.refused.store(true, Ordering::Relaxed);
                self.gather(column, ty, &self.place(rids)?)
            }
            Shape::Many(parts, picks) => {
                // Each part's own rows first, which is where the part's form is dealt with: a
                // packed part unpacks the rows asked for and no others, and a dictionary keeps its
                // values and gathers its codes. What is left to interleave is a few rows a part.
                let mut pieces = Vec::with_capacity(parts.len());
                for (part, offsets) in parts {
                    let Some(piece) = self.rows_in(column, *part, offsets)? else {
                        return Ok(None);
                    };
                    pieces.push(piece);
                }
                let held: Vec<&Vector> = pieces.iter().collect();
                if let Some(picked) = picked(ty, &held, picks)? {
                    return Ok(Some(picked));
                }
                // A form or a type the pick does not read, a list or a struct or string views, a
                // value at a time. Nothing a link join has been asked to gather so far is one.
                let values: Vec<Value> = picks
                    .iter()
                    .map(|&(piece, row)| {
                        held.get(piece as usize)
                            .map_or(Value::Null, |piece| piece.value_at(row as usize))
                    })
                    .collect();
                Vector::from_values(ty.clone(), &values).map(Some)
            }
        }
    }

    /// The first row of each part, and the row count after the last.
    fn starts(&self) -> Result<&Directory> {
        if let Some(directory) = self.starts.get() {
            return Ok(directory);
        }
        let parts = self.rows.chunk_count();
        let mut starts = Vec::with_capacity(parts + 1);
        let mut at = 0u64;
        starts.push(at);
        for part in 0..parts {
            at += self.rows.chunk_len(part)? as u64;
            starts.push(at);
        }
        Ok(self.starts.get_or_init(|| Directory::new(starts)))
    }

    /// The rows of one part at `offsets`, with [`NO_ROW`] as a null, or `None` past the budget.
    ///
    /// A part already held is gathered from. A part the offsets take a real share of, or one an
    /// earlier chunk already came to, is read whole and held, since the chunks after are likely to
    /// want it too: that is a child stored in its parent's order, or one whose survivors come back
    /// to the same parts chunk after chunk. A part a chunk takes a few rows of the first time it
    /// comes to it is read at those rows alone and not held, which is a filtered child walking
    /// forwards through its parent and leaving each part behind. Reading a whole string part there
    /// decoded every value of it for the one or two a chunk wanted, and on TPC-H q12 that was every
    /// `o_orderpriority` of `orders` again. A second visit reads whole because a read of a few rows
    /// still reads and checks the part's pages, and on q09 each chunk took a few rows of every
    /// `partsupp` part, so reading them a few at a time read every page once per chunk.
    fn rows_in(&self, column: usize, part: usize, offsets: &[u32]) -> Result<Option<Vector>> {
        if let Some(held) = self.held_part(column, part) {
            return held.gather(offsets).map(Some);
        }
        let mut positions: Vec<u32> =
            offsets.iter().copied().filter(|&offset| offset != NO_ROW).collect();
        positions.sort_unstable();
        positions.dedup();
        let length = self.rows.chunk_len(part)?;
        let first = {
            let mut visited = self.visited.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
            visited.insert((column, part))
        };
        if !first || positions.len() * SPARSE >= length {
            let Some(source) = self.part(column, part)? else {
                return Ok(None);
            };
            return source.gather(offsets).map(Some);
        }
        let read = self.rows.read_selected(part, &[column], &positions)?;
        let read = read.column(0)?;
        // Each offset as its place among the positions read, which rise, so a search finds it.
        let places: Vec<u32> = offsets
            .iter()
            .map(|offset| {
                // Under the chunk's length, which is a `u32` because a row id is.
                positions.binary_search(offset).map_or(NO_ROW, |place| place as u32)
            })
            .collect();
        read.gather(&places).map(Some)
    }

    /// One part of one column, if a gather has already read it whole.
    fn held_part(&self, column: usize, part: usize) -> Option<Arc<Vector>> {
        let slots = {
            let parts = self.parts.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
            Arc::clone(parts.get(&column)?)
        };
        let slot = slots.get(part)?.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
        slot.as_ref().map(Arc::clone)
    }

    /// One part of one column as the writer stored it, read at most once.
    fn part(&self, column: usize, part: usize) -> Result<Option<Arc<Vector>>> {
        let slots = {
            let mut parts = self.parts.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
            let count = self.rows.chunk_count();
            Arc::clone(
                parts
                    .entry(column)
                    .or_insert_with(|| (0..count).map(|_| Mutex::new(None)).collect()),
            )
        };
        let slot =
            slots.get(part).ok_or_else(|| Error::internal("a gather named a missing part"))?;
        // Held across the read, so two instances that want the same part read it once.
        let mut slot = slot.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
        if let Some(found) = slot.as_ref() {
            return Ok(Some(Arc::clone(found)));
        }
        // Kept in the form it was stored in. Decoding a whole part here cost TPC-H q12 more than
        // the whole column read it replaced, since its survivors reach every part of `orders` and
        // each of them wants one value out of a part of thousands.
        let piece = self.rows.read(part, &[column])?.column(0)?.clone();
        let cost = piece.footprint();
        let before = self.spent_parts.fetch_add(cost, Ordering::Relaxed);
        let held = self.held.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
        if before.saturating_add(cost).saturating_add(spent(&held)) > self.budget {
            self.spent_parts.fetch_sub(cost, Ordering::Relaxed);
            return Ok(None);
        }
        drop(held);
        let piece = Arc::new(piece);
        *slot = Some(Arc::clone(&piece));
        Ok(Some(piece))
    }

    /// The whole of one column, or `None` if reading it would go past the budget.
    ///
    /// The type is the caller's because the table's field list is the caller's. Passing the wrong
    /// one is caught by [`rudb_vector::concat()`], which refuses pieces that do not agree with it.
    ///
    /// # Errors
    ///
    /// If a part of the column cannot be read, or if the parts do not lay end to end.
    pub fn column(&self, column: usize, ty: &LogicalType) -> Result<Option<Arc<Vector>>> {
        self.column_on(column, ty, &serially)
    }

    /// The same, reading the parts on whatever threads `spread` has.
    ///
    /// This is the one a link join calls, and the difference it makes is the whole reason it exists.
    /// A parent column is read once per query in [`Stream::prepare`], before any instance of the
    /// pipeline starts, so every nanosecond of it is on the pipeline's wall clock with nothing else
    /// happening. On TPC-H q12 at scale factor one that read was two thirds of the link plan's wall
    /// clock while the hash join it was being compared against built its table on the whole lease.
    ///
    /// [`Stream::prepare`]: https://docs.rs/rudb-pipeline
    ///
    /// # Errors
    ///
    /// If a part of the column cannot be read, or if the parts do not lay end to end. A part that
    /// failed is reported in part order rather than in the order the threads finished, so the same
    /// table reports the same error however the parts were shared out.
    pub fn column_on(
        &self,
        column: usize,
        ty: &LogicalType,
        spread: &Spread<'_>,
    ) -> Result<Option<Arc<Vector>>> {
        // The lock is held across the read, which serializes two threads that want the same column
        // of the same parent. That is the intended trade: the alternative is both of them reading
        // it, and the column is the expensive thing here while the wait is one read of it.
        let mut held = self.held.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
        if let Some(found) = held.get(&column) {
            return Ok(found.clone());
        }
        let read = self.read(column, ty, &held, spread)?;
        held.insert(column, read.clone());
        Ok(read)
    }

    /// How many bytes the columns held here cost between them.
    ///
    /// For the metrics document, and for a test to assert that a refusal refused.
    #[must_use]
    pub fn footprint(&self) -> usize {
        let held = self.held.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
        spent(&held) + self.spent_parts.load(Ordering::Relaxed)
    }

    /// Reads every part of one column and lays them end to end.
    fn read(
        &self,
        column: usize,
        ty: &LogicalType,
        held: &HashMap<usize, Option<Arc<Vector>>>,
        spread: &Spread<'_>,
    ) -> Result<Option<Arc<Vector>>> {
        let room = self.budget.saturating_sub(spent(held));
        let parts = self.rows.chunk_count();
        // A slot per part rather than one growing list, because the parts may be read in any order
        // and the run they make is in part order. A slot left empty is a part that said nothing or
        // one nobody reached, and the walk below tells those apart from a part that failed.
        let slots: Vec<Mutex<Option<Result<Option<Vector>>>>> =
            (0..parts).map(|_| Mutex::new(None)).collect();
        let cost = AtomicUsize::new(0);
        let over = AtomicBool::new(false);
        let task = |part: usize| {
            // Whoever went past the budget has already decided the answer, so there is no reason to
            // decode anything else. In a serial read that made the column cost one part; in a
            // parallel one it is one part per thread, because the threads already reading cannot be
            // called back. That is a bounded overshoot of a column that is being given up on.
            if over.load(Ordering::Relaxed) {
                return;
            }
            let read = self.piece(part, column);
            let footprint = match &read {
                Ok(Some(piece)) => piece.footprint(),
                Ok(None) | Err(_) => 0,
            };
            // Measured after the flattening, because the number the budget is about is what the
            // column costs once it is a vector, and added up as the parts arrive rather than at the
            // end so that a column far past the budget is given up on early.
            if cost.fetch_add(footprint, Ordering::Relaxed).saturating_add(footprint) > room {
                over.store(true, Ordering::Relaxed);
                return;
            }
            let mut slot = slots[part].lock().unwrap_or_else(|poisoned| poisoned.into_inner());
            *slot = Some(read);
        };
        spread(parts, &task)?;

        let mut pieces = Vec::with_capacity(parts);
        for slot in &slots {
            let taken = slot.lock().unwrap_or_else(|poisoned| poisoned.into_inner()).take();
            match taken {
                // A part that failed fails the column, and the first one in part order is the one
                // reported, which is why the walk is a walk rather than a look at whoever finished
                // last.
                Some(Err(error)) => return Err(error),
                Some(Ok(piece)) => pieces.extend(piece),
                // Nothing here is not a failure. Either the part had no rows, or the budget went
                // while this one was still unread, and the test below is what tells those apart.
                None => {}
            }
        }
        if over.load(Ordering::Relaxed) {
            return Ok(None);
        }
        if let Some(whole) = coded(&pieces)? {
            return Ok((whole.footprint() <= room).then(|| Arc::new(whole)));
        }
        // Not codes into one dictionary all the way down, so the pieces that were left as codes on
        // the chance they were are strings like the rest.
        for piece in &mut pieces {
            if piece.form() != Form::Flat {
                // flatten: the pieces do not share one dictionary, so the column is built as strings.
                *piece = piece.flatten()?;
            }
        }
        // On the same threads, because laying the parts end to end is a copy of every byte of the
        // column and it happens in the same place the reads do, with the pipeline stopped. On the
        // string column of q12's parent projection it measured as large as the parallel part reads
        // it follows, 18 to 73 ms against 27 to 84 ms.
        let Some(whole) = concat_on(ty, &pieces, spread)? else {
            return Ok(None);
        };
        // Measured again on the result, because laying the pieces end to end is where a string
        // column's arena is sized and the answer is not the sum of the pieces.
        if whole.footprint() > room {
            return Ok(None);
        }
        // The pieces go before the paging and not after it, and the order is the whole of whether
        // the paging happens. `into_pages` moves a string arena into a page only when the column it
        // is paging is the one holder of it, because the other way to do it is to copy the arena and
        // copying is what this is here to avoid. With one part, `concat` hands back that part's own
        // arena, so a `pieces` still in scope is a second holder and the paging quietly declines.
        drop(pieces);
        // Paged, because this is the definition of a column that is handed out many times: it is
        // read once here and then gathered from by every chunk of the child for the rest of the
        // query. The form that cares is the string body. A flatten of a gather over one takes a
        // handle to the arena when the arena is a page and copies the bytes of every string it
        // reached when it is not. Without this line every chunk copies, and on TPC-H q12 at scale
        // factor one that was fourteen hundred copies a query out of a column of five distinct
        // values.
        Ok(Some(Arc::new(whole.into_pages())))
    }

    /// One part of one column, decoded unless it is codes into the table's dictionary, or nothing
    /// when the part holds no rows.
    ///
    /// Everything expensive about reading a parent column is in here, which is why this is the unit
    /// the threads are shared out over: the read of the part and the decode of whatever encoding its
    /// writer chose. It touches nothing but its own part.
    fn piece(&self, part: usize, column: usize) -> Result<Option<Vector>> {
        let chunk = self.rows.read(part, &[column])?;
        let piece = chunk.column(0)?;
        // A part with no rows in it contributes no rows to the run and would make `concat` decline
        // the whole of it, which is a column given up on over a part that says nothing.
        if piece.is_empty() {
            return Ok(None);
        }
        // Codes into the table's one dictionary are kept as codes, because a whole column of them
        // is a column the gather can index as well, and [`coded`] lays them end to end.
        if piece.form() == Form::Flat || piece.stable_dictionary_parts().is_some() {
            return Ok(Some(piece.clone()));
        }
        // flatten: the whole point of this type is a run the gather can index by a row id of the
        // parent table, and a row id has no meaning against a bit packed part that has not been
        // decoded. See the module doc for why this is a decode the hash join pays as well.
        Ok(Some(piece.flatten()?))
    }
}

/// The pieces of a column as one run of codes into the dictionary they all share, or `None` when
/// they do not all share one.
///
/// A native table keeps a column of few distinct strings as codes into one dictionary for the whole
/// table, and every part of it points at the same values. Flattening those turned TPC-H q12's
/// `o_orderpriority`, five values over a million and a half orders, into a million and a half
/// strings, and every chunk above the link join then compared the strings where a code would have
/// done. The hash join it replaces already keeps the codes, see `spec/perf/39-codes-through-the-join.md`.
fn coded(pieces: &[Vector]) -> Result<Option<Vector>> {
    let mut shared: Option<&Arc<Vector>> = None;
    let mut rows = 0;
    let mut nulls = false;
    for piece in pieces {
        let Some((codes, values)) = piece.stable_dictionary_parts() else { return Ok(None) };
        if codes.len() != piece.len() || shared.is_some_and(|held| !Arc::ptr_eq(held, values)) {
            return Ok(None);
        }
        shared = Some(values);
        rows += codes.len();
        nulls |= piece.validity().has_nulls(piece.len());
    }
    let Some(values) = shared else { return Ok(None) };
    let mut codes = Vec::with_capacity(rows);
    let mut valid = Vec::with_capacity(if nulls { rows } else { 0 });
    for piece in pieces {
        let Some((run, _)) = piece.stable_dictionary_parts() else { return Ok(None) };
        codes.extend_from_slice(run);
        if nulls {
            let validity = piece.validity();
            valid.extend((0..run.len()).map(|row| validity.is_valid(row)));
        }
    }
    let vector = Vector::stable_dictionary(codes, Arc::clone(values))?;
    Ok(Some(if nulls {
        vector.with_validity(Validity::from_iter(rows, |row| valid[row]))
    } else {
        vector
    }))
}

/// What the columns already held cost between them.
fn spent(held: &HashMap<usize, Option<Arc<Vector>>>) -> usize {
    held.values().flatten().map(|column| column.footprint()).sum()
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use rudb_common::{LogicalType, Result, Value};
    use rudb_storage::MemoryTable;
    use rudb_vector::{Chunk, NO_ROW, Validity, Vector};

    use super::{Directory, Parent};
    use crate::table::Rows;

    /// The directory names the same part a search over the starts does, for every row id, over
    /// parts of uneven length, empty ones among them, and a short last one.
    #[test]
    fn the_directory_finds_the_part_a_search_finds() {
        let layouts: [&[u64]; 5] = [
            &[10],
            &[4, 4, 4, 1],
            &[7, 0, 3, 9, 0, 0, 2],
            &[65_536, 65_536, 65_536, 12],
            &[1, 100, 1, 100, 5],
        ];
        for lengths in layouts {
            let mut starts = vec![0u64];
            for &len in lengths {
                starts.push(starts.last().copied().unwrap_or(0) + len);
            }
            let total = *starts.last().expect("a row count");
            let directory = Directory::new(starts.clone());
            for at in 0..total {
                let searched = starts.partition_point(|&start| start <= at) - 1;
                assert_eq!(directory.part_of(at), searched, "{lengths:?} at {at}");
            }
        }
    }

    /// A table of one integer column, written `per` rows to a chunk, so that a column of it is
    /// genuinely several parts rather than one.
    fn table(values: &[i32], per: usize) -> Rows {
        let mut rows = MemoryTable::new(vec![LogicalType::Integer]);
        for group in values.chunks(per) {
            let held: Vec<Value> = group.iter().map(|&value| Value::Integer(value)).collect();
            let column = Vector::from_values(LogicalType::Integer, &held).expect("a column");
            rows.append(Chunk::new(vec![column]).expect("a chunk")).expect("appended");
        }
        Rows::Memory(rows)
    }

    /// The same table with one string column, which is the form the paging below is about.
    fn strings(values: &[&str], per: usize) -> Rows {
        let mut rows = MemoryTable::new(vec![LogicalType::Varchar]);
        for group in values.chunks(per) {
            let held: Vec<Value> =
                group.iter().map(|value| Value::Varchar((*value).to_string())).collect();
            let column = Vector::from_values(LogicalType::Varchar, &held).expect("a column");
            rows.append(Chunk::new(vec![column]).expect("a chunk")).expect("appended");
        }
        Rows::Memory(rows)
    }

    /// A string column comes back over a page, which is what keeps a link join from copying the
    /// bytes it gathers once per chunk of the child.
    ///
    /// Asserted here rather than left to the vector crate because the thing that can break it is
    /// local: `into_pages` declines an arena that has another holder, so a piece of the read left
    /// alive would turn this into a silent no change with every test still green and q12 still slow.
    ///
    /// Several parts, because that is the read that lays an arena out and the one a parent worth
    /// gathering from has. A column that arrived in one part is handed back as that part and keeps
    /// whatever form the part was in, which for a stored column is already over a page.
    #[test]
    fn a_string_column_comes_back_over_a_page_rather_than_an_arena_of_its_own() {
        let values = ["1-URGENT", "2-HIGH", "3-MEDIUM", "4-NOT SPECIFIED", "5-LOW"];
        let held: Vec<&str> = (0..500).map(|row| values[row % values.len()]).collect();
        for per in [250, 64] {
            let parent = Parent::new(strings(&held, per), 64 * 1024 * 1024);
            let column = parent.column(0, &LogicalType::Varchar).expect("read").expect("it fits");
            let (_, arena) = column.shared_views().expect("a string column is string views");
            assert!(arena.is_shared(), "{per} rows a part came back over an arena of its own");
            assert_eq!(column.len(), 500);
            assert_eq!(column.value_at(499), Value::Varchar("5-LOW".into()));
        }
    }

    /// Codes into one table wide dictionary come back as codes, in row order and with their nulls,
    /// and codes that do not all share a dictionary come back as strings.
    #[test]
    fn codes_into_one_dictionary_stay_codes_and_two_dictionaries_become_strings() {
        let words = |words: &[&str]| {
            let values: Vec<Value> =
                words.iter().map(|&word| Value::Varchar(word.into())).collect();
            Arc::new(Vector::from_values(LogicalType::Varchar, &values).expect("strings"))
        };
        let coded = |codes: &[u32], values: &Arc<Vector>, null: Option<usize>| {
            Vector::stable_dictionary(codes.to_vec(), Arc::clone(values))
                .expect("codes inside the dictionary")
                .with_validity(Validity::from_iter(codes.len(), |row| Some(row) != null))
        };
        let values = words(&["1-URGENT", "2-HIGH", "5-LOW"]);
        let parts = |second: &Arc<Vector>| {
            let mut rows = MemoryTable::new(vec![LogicalType::Varchar]);
            for column in [coded(&[2, 0, 1], &values, None), coded(&[1, 1], second, Some(0))] {
                rows.append(Chunk::new(vec![column]).expect("a chunk")).expect("appended");
            }
            Rows::Memory(rows)
        };
        let text = |word: &str| Value::Varchar(word.into());
        let want = [text("5-LOW"), text("1-URGENT"), text("2-HIGH"), Value::Null, text("2-HIGH")];

        let parent = Parent::new(parts(&values), 64 * 1024 * 1024);
        let column = parent.column(0, &LogicalType::Varchar).expect("read").expect("it fits");
        let (codes, held) = column.stable_dictionary_parts().expect("still codes");
        assert!(Arc::ptr_eq(held, &values), "the codes point somewhere else");
        assert_eq!(codes.len(), 5);
        let read: Vec<Value> = (0..5).map(|row| column.value_at(row)).collect();
        assert_eq!(read, want);

        let other = words(&["1-URGENT", "2-HIGH", "5-LOW"]);
        let parent = Parent::new(parts(&other), 64 * 1024 * 1024);
        let column = parent.column(0, &LogicalType::Varchar).expect("read").expect("it fits");
        assert!(column.stable_dictionary_parts().is_none(), "two dictionaries are not one");
        let read: Vec<Value> = (0..5).map(|row| column.value_at(row)).collect();
        assert_eq!(read, want);
    }

    /// The thing the whole module exists for: the parts of a column come back as one vector, in
    /// the order the rows are stored in, because a `rid` is an offset into that order.
    #[test]
    fn a_column_read_in_parts_comes_back_as_one_vector_in_row_order() {
        let values: Vec<i32> = (0..1000).collect();
        let parent = Parent::new(table(&values, 128), 64 * 1024 * 1024);
        let column = parent.column(0, &LogicalType::Integer).expect("read").expect("it fits");
        assert_eq!(column.len(), 1000);
        assert_eq!(column.value_at(0), Value::Integer(0));
        assert_eq!(column.value_at(999), Value::Integer(999));
        assert_eq!(column.value_at(500), Value::Integer(500), "part boundaries do not renumber");
    }

    /// Asked twice, read once, and the same allocation both times. A link join asks per chunk of
    /// the child, so a column that were read per ask would be read a thousand times.
    #[test]
    fn a_column_asked_for_twice_is_the_same_allocation() {
        let parent = Parent::new(table(&(0..64).collect::<Vec<i32>>(), 16), 64 * 1024 * 1024);
        let first = parent.column(0, &LogicalType::Integer).expect("read").expect("it fits");
        let second = parent.column(0, &LogicalType::Integer).expect("read").expect("it fits");
        assert!(Arc::ptr_eq(&first, &second), "the second ask read the column again");
    }

    /// A refusal is `None` and not an error, because the caller's answer to `None` is to run the
    /// hash join, and the query answers the same either way.
    #[test]
    fn a_column_that_does_not_fit_the_budget_is_refused_rather_than_failed() {
        let values: Vec<i32> = (0..4096).collect();
        let parent = Parent::new(table(&values, 512), 64);
        assert_eq!(
            parent.column(0, &LogicalType::Integer).expect("no error"),
            None,
            "a column far past the budget is refused"
        );
        assert_eq!(parent.footprint(), 0, "and nothing is held on to afterwards");
    }

    /// The budget is over the columns together and not over each one, because what the gather
    /// costs is the parent's projection rather than any one column of it.
    #[test]
    fn the_budget_is_shared_between_the_columns_of_one_parent() {
        let mut rows = MemoryTable::new(vec![LogicalType::Integer, LogicalType::Integer]);
        // Two parts of a thousand rows each, so that each column is laid end to end into a run of
        // its own. A column of one part comes back as a share of the table's page, and since #1491
        // a share of a page the table is holding anyway is charged as the share it is.
        for part in 0..2 {
            let held: Vec<Value> = (part * 1000..part * 1000 + 1000).map(Value::Integer).collect();
            let first = Vector::from_values(LogicalType::Integer, &held).expect("a column");
            let second = Vector::from_values(LogicalType::Integer, &held).expect("a column");
            rows.append(Chunk::new(vec![first, second]).expect("a chunk")).expect("appended");
        }
        let rows = Rows::Memory(rows);
        // Room for one column of eight thousand bytes and not for two.
        let parent = Parent::new(rows, 12 * 1024);
        assert!(parent.column(0, &LogicalType::Integer).expect("read").is_some());
        assert_eq!(
            parent.column(1, &LogicalType::Integer).expect("no error"),
            None,
            "the second column is refused because the first one is still held"
        );
    }

    /// A [`Spread`] that runs each part on a thread of its own, in no particular order.
    ///
    /// Not what the engine passes, which shares the parts out over a fixed lease off a counter. This
    /// is the harsher version on purpose: a thread per part and nothing deciding who goes first is
    /// the widest the interleaving can get, so anything the read does that depends on part order
    /// happening to be arrival order shows up here.
    fn scattered(count: usize, task: &(dyn Fn(usize) + Sync)) -> Result<()> {
        std::thread::scope(|scope| {
            let running: Vec<_> =
                (0..count).rev().map(|at| scope.spawn(move || task(at))).collect();
            for thread in running {
                thread.join().expect("a part reader panicked");
            }
        });
        Ok(())
    }

    /// The point of the parallel read: the same column, whichever thread read which part.
    ///
    /// Row order is the whole of correctness here, because a `rid` is an offset into it, and the read
    /// no longer appends the parts in the order it read them. So this asserts the order rather than
    /// just the contents, at every part boundary and at both ends.
    #[test]
    fn a_column_read_on_many_threads_comes_back_in_the_same_order_as_one_read_on_one() {
        let values: Vec<i32> = (0..1000).collect();
        let one = Parent::new(table(&values, 128), 64 * 1024 * 1024);
        let many = Parent::new(table(&values, 128), 64 * 1024 * 1024);
        let serial = one.column(0, &LogicalType::Integer).expect("read").expect("it fits");
        let parallel =
            many.column_on(0, &LogicalType::Integer, &scattered).expect("read").expect("it fits");
        assert_eq!(parallel.len(), serial.len());
        for row in 0..serial.len() {
            assert_eq!(parallel.value_at(row), serial.value_at(row), "row {row} moved");
        }
    }

    /// A string column too, because that one is laid out rather than copied and the arena is built
    /// from the pieces in the order the walk found them.
    #[test]
    fn a_string_column_read_on_many_threads_comes_back_in_row_order_and_over_a_page() {
        let values = ["1-URGENT", "2-HIGH", "3-MEDIUM", "4-NOT SPECIFIED", "5-LOW"];
        let held: Vec<&str> = (0..500).map(|row| values[row % values.len()]).collect();
        let parent = Parent::new(strings(&held, 64), 64 * 1024 * 1024);
        let column =
            parent.column_on(0, &LogicalType::Varchar, &scattered).expect("read").expect("it fits");
        let (_, arena) = column.shared_views().expect("a string column is string views");
        assert!(arena.is_shared(), "a parallel read came back over an arena of its own");
        assert_eq!(column.len(), 500);
        for (row, want) in held.iter().enumerate() {
            assert_eq!(column.value_at(row), Value::Varchar((*want).into()), "row {row} moved");
        }
    }

    /// The budget still refuses, and still holds nothing afterwards, when the parts arrive at once.
    ///
    /// This is the case the parallel read changes the most. Serially the first part past the budget
    /// ends the read; here every thread may be holding a part by the time one of them notices. What
    /// has to survive is the answer, which is a refusal rather than a short column.
    #[test]
    fn a_column_past_the_budget_is_refused_however_many_threads_read_it() {
        let values: Vec<i32> = (0..4096).collect();
        let parent = Parent::new(table(&values, 512), 64);
        assert_eq!(
            parent.column_on(0, &LogicalType::Integer, &scattered).expect("no error"),
            None,
            "a column far past the budget is refused"
        );
        assert_eq!(parent.footprint(), 0, "and nothing is held on to afterwards");
    }

    /// A chunk whose ids all land in one part reads that part alone, and a chunk
    /// whose ids land in several comes back with every row in the place it asked for.
    #[test]
    fn a_gather_by_part_reads_the_rows_asked_for_in_the_order_asked() {
        let values: Vec<i32> = (0..1000).collect();
        let parent = Parent::new(table(&values, 128), 64 * 1024 * 1024);
        let one = parent.place(&[130, NO_ROW, 129, 255]).expect("placed");
        assert_eq!(one.parts(), 1);
        let column = parent.gather(0, &LogicalType::Integer, &one).expect("read").expect("fits");
        assert_eq!(column.len(), 4, "the rows asked for and not the part");
        let want = [Value::Integer(130), Value::Null, Value::Integer(129), Value::Integer(255)];
        for (row, want) in want.iter().enumerate() {
            assert_eq!(&column.value_at(row), want, "row {row}");
        }

        let ids = [999, 0, NO_ROW, 500, 1, 998, 128];
        let many = parent.place(&ids).expect("placed");
        assert_eq!(many.parts(), 4);
        let column = parent.gather(0, &LogicalType::Integer, &many).expect("read").expect("fits");
        assert_eq!(column.len(), ids.len());
        for (row, &id) in ids.iter().enumerate() {
            let want = if id == NO_ROW { Value::Null } else { Value::Integer(id as i32) };
            assert_eq!(column.value_at(row), want, "row {row}");
        }
    }

    /// A chunk that reaches more than half the parts is gathered out of the column read whole, and
    /// comes back the same as one gathered by part would.
    #[test]
    fn a_chunk_over_most_of_the_parts_is_gathered_out_of_the_whole_column() {
        let values: Vec<i32> = (0..1024).collect();
        let parent = Parent::new(table(&values, 128), 64 * 1024 * 1024);
        let ids = [1000, 3, NO_ROW, 300, 700, 129, 900, 500, 3];
        let placed = parent.place(&ids).expect("placed");
        assert_eq!(placed.parts(), 6);
        let column = parent.gather(0, &LogicalType::Integer, &placed).expect("read").expect("fits");
        for (row, &id) in ids.iter().enumerate() {
            let want = if id == NO_ROW { Value::Null } else { Value::Integer(id as i32) };
            assert_eq!(column.value_at(row), want, "row {row}");
        }
        let whole = Parent::new(table(&values, 128), 64 * 1024 * 1024);
        whole.column(0, &LogicalType::Integer).expect("read").expect("fits");
        assert_eq!(parent.footprint(), whole.footprint(), "the column is held once, end to end");

        let held: Vec<&str> = (0..500).map(|row| ["a", "bb", "ccc"][row % 3]).collect();
        let parent = Parent::new(strings(&held, 64), 64 * 1024 * 1024);
        let ids = [499, 3, 64, 200, 130, 260, 330, 400];
        let placed = parent.place(&ids).expect("placed");
        let column = parent.gather(0, &LogicalType::Varchar, &placed).expect("read").expect("fits");
        for (row, &id) in ids.iter().enumerate() {
            assert_eq!(column.value_at(row), Value::Varchar(held[id as usize].into()), "row {row}");
        }
    }

    /// A column the budget will not hold whole is gathered by part instead, for that chunk and for
    /// the ones after it.
    #[test]
    fn a_chunk_over_most_of_the_parts_goes_by_part_when_the_column_does_not_fit() {
        let values: Vec<i32> = (0..1024).collect();
        let whole = Parent::new(table(&values, 128), 64 * 1024 * 1024);
        whole.column(0, &LogicalType::Integer).expect("read").expect("fits");
        let parent = Parent::new(table(&values, 128), whole.footprint() - 1);
        let ids: Vec<u32> = (0..8).map(|part| part * 128 + 5).rev().collect();
        for _ in 0..2 {
            let placed = parent.place(&ids).expect("placed");
            let column =
                parent.gather(0, &LogicalType::Integer, &placed).expect("read").expect("fits");
            for (row, &id) in ids.iter().enumerate() {
                assert_eq!(column.value_at(row), Value::Integer(id as i32), "row {row}");
            }
        }
        assert!(
            parent.refused.load(std::sync::atomic::Ordering::Relaxed),
            "the whole read is not tried again"
        );
    }

    /// A chunk that takes a few rows of a part leaves nothing held, and one that takes a share of
    /// it leaves the part held for the next.
    #[test]
    fn a_gather_by_part_holds_a_part_it_takes_a_share_of_and_not_one_it_takes_a_few_rows_of() {
        let values: Vec<i32> = (0..1024).collect();
        let parent = Parent::new(table(&values, 128), 64 * 1024 * 1024);
        let few = parent.place(&[3, 5, 7]).expect("placed");
        let column = parent.gather(0, &LogicalType::Integer, &few).expect("read").expect("fits");
        assert_eq!(column.value_at(2), Value::Integer(7));
        assert_eq!(parent.footprint(), 0, "a few rows are read and not held");
        let ids: Vec<u32> = (256..384).rev().collect();
        let share = parent.place(&ids).expect("placed");
        let column = parent.gather(0, &LogicalType::Integer, &share).expect("read").expect("fits");
        assert_eq!(column.value_at(0), Value::Integer(383));
        assert!(parent.footprint() > 0, "a whole part is held for the chunks after");
        let again = parent.place(&[300]).expect("placed");
        let column = parent.gather(0, &LogicalType::Integer, &again).expect("read").expect("fits");
        assert_eq!(column.value_at(0), Value::Integer(300), "and gathered from once held");
    }

    /// A second chunk that comes back to a part it took a few rows of reads it whole and holds it,
    /// which is a child whose survivors come back to the same parts every chunk.
    #[test]
    fn a_gather_by_part_holds_a_part_a_second_chunk_comes_back_to() {
        let values: Vec<i32> = (0..1024).collect();
        let parent = Parent::new(table(&values, 128), 64 * 1024 * 1024);
        for (chunk, id) in [4u32, 8, 12].into_iter().enumerate() {
            let placed = parent.place(&[id]).expect("placed");
            let column =
                parent.gather(0, &LogicalType::Integer, &placed).expect("read").expect("fits");
            assert_eq!(column.value_at(0), Value::Integer(id as i32));
            assert_eq!(parent.footprint() > 0, chunk > 0, "held from the second chunk on");
        }
    }

    /// Only the parts asked for are read, which is the whole point of reading by part.
    #[test]
    fn a_gather_by_part_reads_no_part_it_was_not_asked_for() {
        let values: Vec<i32> = (0..1024).collect();
        let parent = Parent::new(table(&values, 128), 64 * 1024 * 1024);
        let placed = parent.place(&[3, 5]).expect("placed");
        parent.gather(0, &LogicalType::Integer, &placed).expect("read").expect("fits");
        let one = parent.footprint();
        let whole = Parent::new(table(&values, 128), 64 * 1024 * 1024);
        whole.column(0, &LogicalType::Integer).expect("read").expect("fits");
        assert!(one * 4 < whole.footprint(), "{one} bytes read for one part of eight");
    }

    /// A chunk with no parent at all is nulls, and a string column over several parts comes back
    /// right too, since strings are the one layout the pick copies a value at a time.
    #[test]
    fn a_gather_by_part_of_nothing_is_null_and_of_strings_is_the_strings() {
        let values = ["1-URGENT", "2-HIGH", "3-MEDIUM", "4-NOT SPECIFIED", "5-LOW"];
        let held: Vec<&str> = (0..500).map(|row| values[row % values.len()]).collect();
        let parent = Parent::new(strings(&held, 64), 64 * 1024 * 1024);
        let none = parent.place(&[NO_ROW, NO_ROW]).expect("placed");
        let column = parent.gather(0, &LogicalType::Varchar, &none).expect("read").expect("fits");
        assert_eq!(column.len(), 2);
        assert_eq!(column.value_at(1), Value::Null);
        let ids = [499, 3, 64, 200];
        let placed = parent.place(&ids).expect("placed");
        let column = parent.gather(0, &LogicalType::Varchar, &placed).expect("read").expect("fits");
        for (row, &id) in ids.iter().enumerate() {
            assert_eq!(column.value_at(row), Value::Varchar(held[id as usize].into()), "row {row}");
        }
    }

    /// Past the end of the table is an error and not a read of some other row.
    #[test]
    fn a_gather_by_part_past_the_end_is_refused() {
        let parent = Parent::new(table(&(0..100).collect::<Vec<i32>>(), 32), 64 * 1024 * 1024);
        assert!(parent.place(&[100]).is_err());
        let tight = Parent::new(table(&(0..4096).collect::<Vec<i32>>(), 512), 64);
        // Every row of the first part, so the part is read whole and the budget is asked.
        let placed = tight.place(&(0..512).collect::<Vec<u32>>()).expect("placed");
        assert_eq!(tight.gather(0, &LogicalType::Integer, &placed).expect("no error"), None);
        assert_eq!(tight.footprint(), 0, "a refused part is not held");
    }
}
