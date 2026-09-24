//! One vector built out of pieces that each answer a different set of its rows.
//!
//! The inverse of [`Vector::gather`]. A gather says where each output row reads from, so it wants
//! one source and a position per row. An assembly says where each input row writes to, so it takes
//! several sources and a position per row of each of them, and the rows no source claims come out
//! null.
//!
//! `CASE` is the shape that wants this. Each arm is evaluated over the rows no earlier arm claimed,
//! which is a correctness rule rather than a performance one, since `CASE WHEN x <> 0 THEN 1 / x
//! ELSE 0 END` divides by zero on the rows the arm excludes if the arm is evaluated for them. So the
//! arms produce several short answers that have to end up interleaved in the order the rows arrived
//! in, and interleaving them is what this is. A join assembling a payload out of a matched side and
//! an unmatched side wants the same thing.
//!
//! # Why it is not a `Vec<Value>`
//!
//! Because that is a heap allocation per string and a drop per string afterwards, on top of the
//! walk through the enum that a `Value` is. On the ClickBench query that groups by a `CASE` over
//! `Referer`, building the answer that way was about a quarter of the whole query: a quarter of the
//! instructions were in `value_at`, the `Value` drop glue, `malloc` and `free`, for an answer whose
//! bytes were already sitting in a string arena and only needed to be pointed at.
//!
//! What happens instead is that the pieces are laid end to end into one run of data and the
//! interleave is then a single gather over that run, which is a typed loop per physical layout and
//! is the same loop [`Vector::gather`] already goes down. A string's bytes are copied once, into one
//! arena that was sized before any of them moved.
//!
//! # The shape of the interface
//!
//! A builder rather than a function taking a slice of pieces, because the caller producing the
//! pieces is usually borrowing scratch space to produce each one and cannot hold two of them at
//! once. [`Assembly::place`] reads a piece and is done with it, so the borrow ends between arms and
//! nothing has to be cloned to keep it alive.
//!
//! # The simpler thing next to it
//!
//! [`concat()`] is the case where the pieces arrive in order and claim every row, which is what a row
//! group of a stored table is built out of. An assembly would answer it, and it would pay for a
//! position per row and a flatten per piece to answer something that is a run of `memcpy`s, so it is
//! its own function. What the two share is the typed append underneath both of them.

use std::collections::{HashMap, HashSet};
use std::ops::Range;
use std::sync::{Arc, Mutex};

use rudb_common::{Error, LogicalType, Result, Spread, Value};

use crate::buffer::Buffer;
use crate::string::{Arenas, StringColumn, StringView};
use crate::validity::Validity;
use crate::vector::{
    Data, Form, NOWHERE, Vector, copy_of, data_for, empty_data_for, layout_of, placed_of,
};

/// A vector being built out of pieces, each landing at the positions it is given.
///
/// Build one with [`Assembly::new`], call [`Assembly::place`] once per piece, and finish it with
/// [`Assembly::finish`]. A row no piece claims is null.
#[derive(Debug)]
pub struct Assembly {
    ty: LogicalType,
    rows: usize,
    /// The pieces laid end to end, for a type that has a flat layout to lay them in.
    data: Data,
    /// Where each output row reads from in `data`, or [`NOWHERE`] for a row no piece claimed.
    at: Vec<usize>,
    /// Whether each output row holds a value rather than a null.
    live: Vec<bool>,
    /// The fallback for the nested types, which have no run of data to lay anything end to end in.
    ///
    /// A `LIST`, a `STRUCT` and a `MAP` are a child vector and a run of entries rather than a run of
    /// values, so laying two of them end to end is not an append to one buffer and the copy loop
    /// this is built around has nothing to walk. They go through values, which is what they did
    /// before this existed and is not a regression for them. Nothing on a ClickBench or TPC-H path
    /// reaches it.
    values: Option<Vec<Value>>,
}

impl Assembly {
    /// An assembly of `rows` rows of `ty`, with every row null until a piece claims it.
    ///
    /// # Errors
    ///
    /// If the type is one there is no flat layout for yet, which today means `ARRAY` and `UNION`.
    pub fn new(ty: LogicalType, rows: usize) -> Result<Self> {
        let nested =
            matches!(ty, LogicalType::List(_) | LogicalType::Struct(_) | LogicalType::Map(_, _));
        let values = if nested { Some(vec![Value::Null; rows]) } else { None };
        let data = if nested { Data::Empty } else { empty_data_for(&ty)? };
        Ok(Self { ty, rows, data, at: vec![NOWHERE; rows], live: vec![false; rows], values })
    }

    /// How many rows the finished vector will have.
    #[must_use]
    pub fn rows(&self) -> usize {
        self.rows
    }

    /// Writes row `n` of `piece` at output row `positions[n]`, for every row of `piece`.
    ///
    /// A row claimed twice takes the value the later call gave it, which is not a case `CASE`
    /// produces, since its arms run over disjoint sets of rows, and is defined rather than left
    /// open so that a caller that does it gets an answer instead of whichever of the two the copy
    /// loop happened to reach.
    ///
    /// # Errors
    ///
    /// If `piece` has a different number of rows than there are positions, if a position is past the
    /// end of the assembly, or if `piece` is not of a layout that can be laid after what is already
    /// there.
    pub fn place(&mut self, positions: &[u32], piece: &Vector) -> Result<()> {
        if positions.len() != piece.len() {
            return Err(Error::internal(format!(
                "a piece of {} rows placed at {} positions",
                piece.len(),
                positions.len()
            )));
        }
        for &row in positions {
            if row as usize >= self.rows {
                return Err(Error::internal(format!(
                    "row {row} placed in an assembly of {} rows",
                    self.rows
                )));
            }
        }
        if let Some(values) = &mut self.values {
            // row at a time: the nested fallback named on the field above. A `LIST`, a `STRUCT` and
            // a `MAP` are a child vector and a run of entries rather than a run of values, so there
            // is no buffer to lay one after another and no typed copy to do the interleave with.
            for (slot, &row) in positions.iter().enumerate() {
                values[row as usize] = piece.value_at(slot);
            }
            return Ok(());
        }
        // flatten: the copy loop that does the interleave reads a run of data, and a piece can
        // arrive constant, dictionary encoded or bit packed. Flattening is itself a typed loop per
        // layout, so writing the piece out once here is what stops it being read a value at a time
        // later, and a piece that is already flat is not copied at all.
        let flat = piece.flatten()?;
        let Some(from) = flat.data() else {
            return Err(Error::internal("a flattened vector with no run of data in it"));
        };
        let start = self.data.len();
        let appended = extend(&mut self.data, from, &mut Arenas::default())?;
        for (slot, &row) in positions.iter().enumerate() {
            let row = row as usize;
            // A piece whose data is empty is the untyped null, so it claims its rows and they are
            // null, which is what leaving them at `NOWHERE` says.
            if slot < appended {
                self.at[row] = start + slot;
                self.live[row] = !piece.is_null_at(slot);
            } else {
                self.at[row] = NOWHERE;
                self.live[row] = false;
            }
        }
        Ok(())
    }

    /// The finished vector.
    ///
    /// # Errors
    ///
    /// If the run of data that came out of the pieces is not one the type can hold.
    pub fn finish(self) -> Result<Vector> {
        if let Some(values) = self.values {
            return Vector::from_values(self.ty, &values);
        }
        // An untyped null has no run of data to gather out of, and a gather over one would give a
        // vector of no values calling itself `rows` long.
        if matches!(self.data, Data::Empty) {
            return Ok(Vector::constant(self.ty, Value::Null, self.rows));
        }
        let validity = Validity::from_run(&self.live);
        // A string column is finished by permuting views rather than by copying bytes. Sixteen
        // bytes a row move and the payload stays in the arena the pieces were appended into, which
        // is the same trade [`crate::vector::Body::Views`] is for a page. It matters most where the
        // permutation is the identity and the whole thing is a move, which is every column of a
        // hash join's gathered side.
        if let Data::Varlen(column) = self.data {
            let (laid, arena) = column.into_parts();
            let arena = Arc::new(arena);
            if straight(&self.at) {
                return Ok(Vector::string_views(self.ty, laid, arena)?.with_validity(validity));
            }
            let views = self
                .at
                .iter()
                .map(|&index| laid.get(index).copied().unwrap_or_else(StringView::empty))
                .collect();
            return Ok(Vector::string_views(self.ty, views, arena)?.with_validity(validity));
        }
        // Row `n` reads position `n`, so the copy below would be a copy of the run onto itself. An
        // assembly whose pieces arrived in order and claimed every row is exactly that, and laying
        // the chunks of a join's gathered side end to end is exactly that.
        if straight(&self.at) {
            return Ok(Vector::flat(self.ty, self.data)?.with_validity(validity));
        }
        let gathered = copy_of(&self.data, &self.at);
        Ok(Vector::flat(self.ty, gathered)?.with_validity(validity))
    }
}

/// Several vectors of one type laid end to end as one page, or `None` for a run this will not lay.
///
/// What a row group of a stored table is built out of. The chunks arrive one at a time and each one
/// is a separate allocation, and holding a hundred and twenty of them is a hundred and twenty places
/// a scan of the column has to jump to instead of one run it walks. So they are copied once, into a
/// page, and every chunk the table hands out afterwards is a window cut out of that page.
///
/// # What it will not lay
///
/// Anything that is neither a flat run nor a stable dictionary over the same shared values is
/// answered with `None` rather than with an error, because a caller that gets one has somewhere to
/// put the pieces and this is a choice about layout rather than a failure. Stable dictionary pieces
/// sharing one value vector are the encoded exception: laying them is just appending their codes.
/// An ordinary dictionary has no cross-piece code-space promise, and flattening it would make it
/// larger and throw away the thing that made it useful, so it is still left to the caller.
///
/// # Strings
///
/// A flat varchar piece owns its arena, so a window cut out of a flat varchar page copies every byte
/// of every long string in the window, which is the whole reason [`Form::StringView`] exists.
/// So the varlen page comes back as views over one shared arena: the bytes are copied once
/// here and never again, and a cut afterwards moves sixteen bytes a row the same way it does for a
/// column of integers.
///
/// # Errors
///
/// If the type has no flat layout, or if a piece holds fewer values than it says it has rows.
pub fn concat<V: AsRef<Vector>>(ty: &LogicalType, pieces: &[V]) -> Result<Option<Vector>> {
    let pieces: Vec<&Vector> = pieces.iter().map(AsRef::as_ref).collect();
    laid(ty, &pieces)
}

/// One row out of one of several vectors per pick, into one flat vector.
///
/// What a link join does with a chunk whose parent rows are in several parts of the parent. Each
/// pick names a source and a row of it, and a source past the end of `sources`, such as
/// [`crate::NO_ROW`], is a null. A source is flat or a dictionary over flat values, which is what a
/// gather out of a stored part hands back for every form the writer uses. The copy is one typed
/// loop over the picks, so a chunk that lands in a hundred parts costs what a chunk that lands in
/// one does, rather than a vector and a lay per part.
///
/// Sources that are all codes into one stable dictionary give codes into it, because a table keeps
/// a column of few distinct strings that way and the kernels above compare codes where they would
/// otherwise compare strings. That was most of TPC-H q12's link join, see
/// `spec/perf/64-parent-codes-through-the-link.md`.
///
/// `None` when a source is in some other form or the type has no flat layout, which the caller
/// answers by flattening what it has. A string is copied a string at a time, since the sources have
/// arenas of their own and a view can point into only one.
///
/// # Errors
///
/// If a source's layout is not the one the type calls for.
pub fn picked(
    ty: &LogicalType,
    sources: &[&Vector],
    picks: &[(u32, u32)],
) -> Result<Option<Vector>> {
    if let Some(codes) = picked_codes(sources, picks)? {
        return Ok(Some(codes));
    }
    // Each source as the flat run its values are in, and the codes into that run for a dictionary.
    let mut leaves: Vec<(&Vector, Option<&[u32]>)> = Vec::with_capacity(sources.len());
    for source in sources {
        match source.form() {
            Form::Flat => leaves.push((source, None)),
            Form::Dictionary => match source.dictionary_parts() {
                Some((codes, values)) if values.form() == Form::Flat => {
                    leaves.push((values, Some(codes)));
                }
                _ => return Ok(None),
            },
            _ => return Ok(None),
        }
    }
    let mut datas = Vec::with_capacity(leaves.len());
    for (leaf, _) in &leaves {
        let Some(data) = leaf.data() else { return Ok(None) };
        datas.push(data);
    }
    // Every pick resolved to a row of its leaf once, so the typed loop below is a load and nothing
    // else. A pick that is null anywhere on the way down is `NO_ROW` from here on.
    let mut live = Vec::with_capacity(picks.len());
    let picks: Vec<(u32, u32)> = picks
        .iter()
        .map(|&(source, row)| {
            let found = sources.get(source as usize).zip(leaves.get(source as usize)).and_then(
                |(held, (leaf, codes))| {
                    let row = row as usize;
                    if row >= held.len() || !held.validity().is_valid(row) {
                        return None;
                    }
                    let at =
                        codes.map_or(Some(row), |codes| codes.get(row).map(|&c| c as usize))?;
                    (at < leaf.len() && leaf.validity().is_valid(at)).then_some(at)
                },
            );
            live.push(found.is_some());
            // Under the leaf's length, which a vector keeps under `u32::MAX` rows.
            found.map_or((crate::NO_ROW, 0), |at| (source, at as u32))
        })
        .collect();
    let first = datas.first().copied();
    macro_rules! picking {
        ($(($variant:ident, $native:ty, $zero:expr)),+ $(,)?) => {
            match first {
                None | Some(Data::Empty) => return Ok(Some(Vector::constant(ty.clone(), Value::Null, picks.len()))),
                $(Some(Data::$variant(_)) => {
                    let mut runs = Vec::with_capacity(datas.len());
                    for data in &datas {
                        let Data::$variant(values) = data else {
                            return Ok(None);
                        };
                        runs.push(values.as_slice());
                    }
                    let mut out: Vec<$native> = Vec::with_capacity(picks.len());
                    out.extend(picks.iter().map(|&(source, row)| {
                        runs.get(source as usize)
                            .and_then(|run| run.get(row as usize))
                            .copied()
                            .unwrap_or($zero)
                    }));
                    Data::$variant(Buffer::from_vec(out))
                })+
                Some(Data::Varlen(_)) => {
                    let mut runs = Vec::with_capacity(datas.len());
                    for data in &datas {
                        let Data::Varlen(values) = data else {
                            return Ok(None);
                        };
                        runs.push(values);
                    }
                    let mut out = StringColumn::with_capacity(picks.len());
                    for &(source, row) in &picks {
                        match runs.get(source as usize) {
                            Some(run) => out.push_from(run, row as usize),
                            None => out.push(""),
                        };
                    }
                    Data::Varlen(out)
                }
            }
        };
    }
    let data = crate::for_each_layout!(fixed, picking);
    let vector = Vector::flat(ty.clone(), data)?;
    Ok(Some(if live.iter().all(|&alive| alive) {
        vector
    } else {
        vector.with_validity(Validity::from_run(&live))
    }))
}

/// The same, copying the pieces of a string column on whatever threads `spread` has.
///
/// # Why this exists at all
///
/// Because of where one caller lays its pieces. A link join reads the columns of its parent table in
/// `Stream::prepare`, which runs once before any instance of the pipeline is handed out, so it runs
/// with the whole thread lease parked and every nanosecond of it is on the pipeline's wall clock.
/// Reading the parts of the column on the lease was #1575. This is the step after it, and on TPC-H
/// q12's parent projection at scale factor one it measured as large as the reads: 18 to 73 ms of
/// laying against 27 to 84 ms of parallel part reads.
///
/// # What it parallelises and what it does not
///
/// Strings whose pieces each own an arena, which is the one case where laying is a copy of every byte
/// of the column rather than a copy of a view a row. Everything else goes down the same path
/// [`concat()`] does, including the two cases that are already nearly free: pieces that share one
/// arena, where laying is the views alone, and pieces that are adjacent windows of one page, where it
/// is a handle. Fixed width pieces are also left serial, on the measurement above: the integer column
/// of the same projection lays in 2 to 12 ms, so the copy is there but it is not what is worth a
/// second code path yet.
///
/// The reason the string case is the expensive one is that a flat varchar piece owns its arena, so the
/// serial walk has to be in order: each piece's views record offsets into the page being built and
/// those offsets depend on where the previous piece ended. Every piece's arena length is known before
/// anything is copied, though, so the offsets can be worked out in one pass over the lengths and then
/// every piece copies its own bytes into its own slice of the page with nothing to wait for.
///
/// # Errors
///
/// What [`concat()`] errors on, and whatever `spread` reports. A piece is never the thing that fails
/// here: by the time the copies start the shape has been checked and a copy into a slice of the right
/// size cannot fail.
pub fn concat_on<V: AsRef<Vector>>(
    ty: &LogicalType,
    pieces: &[V],
    spread: &Spread<'_>,
) -> Result<Option<Vector>> {
    let pieces: Vec<&Vector> = pieces.iter().map(AsRef::as_ref).collect();
    if let Some(strung) = strung(ty, &pieces, spread)? {
        return Ok(Some(strung));
    }
    laid(ty, &pieces)
}

/// String pieces that each own an arena, laid into one page with a piece per task.
///
/// `None` is not a refusal to lay. It says these pieces are not the shape this handles and that
/// [`laid`] should have them, which is every case where laying is not a copy of the bytes.
fn strung(ty: &LogicalType, pieces: &[&Vector], spread: &Spread<'_>) -> Result<Option<Vector>> {
    let Some(columns) = apart(ty, pieces) else {
        return Ok(None);
    };
    // One pass over the lengths, which is the pass that makes the copies independent. `base` is where
    // this piece's bytes land in the page and so is what its views are shifted by, and `from` is
    // where its views land among the views.
    let mut bases = Vec::with_capacity(columns.len());
    let mut bytes = 0usize;
    let mut rows = 0usize;
    for column in &columns {
        bases.push((bytes, rows));
        bytes += column.arena().len();
        rows += column.len();
    }

    // Zeroed rather than grown, so the page is one allocation of the right size and every task has
    // somewhere to write before any of them starts. The zeroing of the arena costs nothing worth
    // measuring because it is whole pages the allocator hands over untouched, and the views are
    // sixteen bytes a row of `memset` that the copy below would be writing over anyway.
    let mut arena = vec![0u8; bytes];
    let mut views = vec![StringView::empty(); rows];
    // Cut into a piece of arena and a piece of views per piece, because a task writing through a
    // shared closure cannot be handed a `&mut` any other way and these are disjoint by construction.
    // The lock is a formality: each one is taken exactly once by exactly one task.
    let mut arena_rest: &mut [u8] = &mut arena;
    let mut views_rest: &mut [StringView] = &mut views;
    let mut slots = Vec::with_capacity(columns.len());
    for column in &columns {
        let (arena_head, arena_tail) = arena_rest.split_at_mut(column.arena().len());
        let (views_head, views_tail) = views_rest.split_at_mut(column.len());
        slots.push(Mutex::new((arena_head, views_head)));
        arena_rest = arena_tail;
        views_rest = views_tail;
    }

    let task = |at: usize| {
        let column = columns[at];
        let (base, _) = bases[at];
        let mut slot = slots[at].lock().unwrap_or_else(|poisoned| poisoned.into_inner());
        let (into_arena, into_views) = &mut *slot;
        into_arena.copy_from_slice(column.arena());
        let base = base as u64;
        for (slot, view) in into_views.iter_mut().zip(column.views()) {
            *slot = view.shifted(base);
        }
    };
    spread(columns.len(), &task)?;
    drop(slots);

    let validity = run_of(pieces, rows);
    let page = Vector::string_views(ty.clone(), views, Arc::new(Buffer::from(arena)))?;
    Ok(Some(page.with_validity(validity)))
}

/// The pieces as string columns, when they are strings that each hold an arena of their own.
///
/// Four things are being asked, and each of them is a case that belongs to [`laid`] rather than a
/// case this does worse.
///
/// More than one piece, because one piece is laid by handing its own arena back and copying it into a
/// page of the same size would be a copy for nothing.
///
/// Flat pieces of this type, which is what [`laid`] requires of the general path anyway, and which
/// rules out the stable dictionary and shared view shapes it answers earlier and more cheaply.
///
/// An arena the piece's own views read nearly all of. [`laid`] copies an arena whole only when it is
/// mostly read and otherwise copies a string at a time, because a filtered cut of a Parquet page
/// would drag the rest of the page along for as long as the result lives. A string at a time still
/// writes a piece's bytes into a piece sized run, so it could be done here too, but a decoded part of
/// a stored column is always entirely read and the other shape is not what this is for.
///
/// Arenas that are all different. Pieces sharing an arena are what a cut up page is, and [`laid`]
/// copies a shared one once and shifts the views of every piece that points into it. Copying it once
/// per piece here would be correct and would use more memory than the serial path, which is not a
/// trade worth making for threads.
fn apart<'a>(ty: &LogicalType, pieces: &[&'a Vector]) -> Option<Vec<&'a StringColumn>> {
    if pieces.len() < 2 {
        return None;
    }
    let mut columns = Vec::with_capacity(pieces.len());
    let mut seen = HashSet::with_capacity(pieces.len());
    for piece in pieces {
        if piece.form() != Form::Flat || piece.logical_type() != ty || piece.is_empty() {
            return None;
        }
        let Some(Data::Varlen(column)) = piece.data() else {
            return None;
        };
        if !column.mostly_read() || !seen.insert(column.arena().as_ptr() as usize) {
            return None;
        }
        columns.push(column);
    }
    Some(columns)
}

/// The body of [`concat()`], over borrowed pieces.
///
/// A caller holding its pieces inside chunks would otherwise clone each one into a list, and a
/// clone of a flat vector that owns its values copies every one of them, which is the copy this
/// function exists to make once.
fn laid(ty: &LogicalType, pieces: &[&Vector]) -> Result<Option<Vector>> {
    if pieces.is_empty() {
        return Ok(None);
    }
    let rows = pieces.iter().map(|piece| piece.len()).sum();
    let shared = pieces[0].stable_dictionary_parts().map(|(_, values)| values).filter(|values| {
        pieces.iter().all(|piece| {
            piece.logical_type() == ty
                && !piece.is_empty()
                && piece
                    .stable_dictionary_parts()
                    .is_some_and(|(_, held)| Arc::ptr_eq(held, values))
        })
    });
    if let Some(values) = shared {
        let mut codes = Vec::with_capacity(rows);
        for piece in pieces {
            if let Some((held, _)) = piece.stable_dictionary_parts() {
                codes.extend_from_slice(held);
            }
        }
        let validity = run_of(pieces, rows);
        return Ok(Some(
            Vector::stable_dictionary(codes, Arc::clone(values))?.with_validity(validity),
        ));
    }
    // String views that all point into one arena, which is what a string column gathered out of a
    // join's build side is, chunk after chunk. Laid end to end they are the same views over the same
    // arena, so sixteen bytes a row move and no string is copied.
    if let Some(arena) = pieces[0].shared_views().map(|(_, arena)| arena).filter(|arena| {
        pieces.iter().all(|piece| {
            piece.logical_type() == ty
                && piece.shared_views().is_some_and(|(_, held)| Arc::ptr_eq(held, arena))
        })
    }) {
        let mut views = Vec::with_capacity(rows);
        for piece in pieces {
            if let Some((held, _)) = piece.shared_views() {
                views.extend_from_slice(held);
            }
        }
        let validity = run_of(pieces, rows);
        return Ok(Some(
            Vector::string_views(ty.clone(), views, Arc::clone(arena))?.with_validity(validity),
        ));
    }
    // Checked before anything is copied, because the fallback is for the caller to keep the pieces
    // it already has and a half built page would be work thrown away.
    let laid = pieces
        .iter()
        .all(|piece| piece.form() == Form::Flat && piece.logical_type() == ty && !piece.is_empty());
    if !laid {
        return Ok(None);
    }
    if let Some(data) = adjoined(pieces) {
        let validity = run_of(pieces, rows);
        return Ok(Some(Vector::flat(ty.clone(), data)?.with_validity(validity)));
    }
    // Sized before the first value moves, so the page is one allocation and holds no more than the
    // rows that went into it. Growing from empty instead ends at the next power of two, which on a
    // full row group is eight thousand values of slack carried for the life of the table.
    let mut data = data_for(ty, rows)?;
    let mut arenas = arenas_of(pieces);
    for piece in pieces {
        let from = piece
            .data()
            .ok_or_else(|| Error::internal("a flat vector with no run of data in it"))?;
        let appended = extend(&mut data, from, &mut arenas)?;
        if appended != piece.len() {
            return Err(Error::internal(format!(
                "a piece of {} rows laid {appended} values end to end",
                piece.len()
            )));
        }
    }
    let validity = run_of(pieces, rows);
    if let Data::Varlen(column) = data {
        let (views, arena) = column.into_parts();
        let page = Vector::string_views(ty.clone(), views, Arc::new(arena))?;
        return Ok(Some(page.with_validity(validity)));
    }
    Ok(Some(Vector::flat(ty.clone(), data)?.with_validity(validity).into_pages()))
}

/// Several vectors of one type laid end to end and then read back in `order`.
///
/// What a sort is. Row `n` of the answer is row `order[n]` of the pieces laid end to end, so the
/// pieces are laid once and the answer is one gather, which writes the answer front to back. An
/// [`Assembly`] answers the same question the other way round, with a position per input row that
/// it scatters into, and that costs it a map of the whole column and a scatter per column, which for
/// a sort is the same permutation worked out again for every column. Here the caller works it out
/// once and every column reads it.
///
/// A string column comes back as views over the one arena its bytes were laid into, the same as
/// [`concat()`] gives, so the gather moves sixteen bytes a row.
///
/// # Errors
///
/// If the type has no layout this can lay, if a piece holds fewer values than it has rows, or if
/// an entry of `order` is past the end of the pieces.
pub fn interleave(ty: &LogicalType, pieces: &[Vector], order: &[usize]) -> Result<Vector> {
    interleave_placed(ty, pieces, order, None)
}

/// The same as [`interleave()`], writing each row where it goes rather than reading each row from
/// where it came when the caller also has `inverse`, the place in the answer of every row laid.
///
/// Which of the two is cheaper is decided by how many ascending runs `order` is made of, and only
/// the caller knows that without a pass of its own. Reading through `order` jumps between the runs,
/// so when there are few of them each row of the answer costs a cache line of the laid column to
/// use eight bytes of it, and with a dozen columns on a dozen threads that is the memory bus full.
/// Writing through `inverse` reads the laid column front to back and writes one stream per run,
/// each of them front to back too. On SF1 `lineitem` sorted by ship month, 84 runs, twelve columns
/// on twelve threads went from 145ms to 50ms in a standalone test of just these loops. With runs
/// in the tens of thousands the two come out even and with a run every few rows the writes are the
/// ones that miss, so a caller with an order like that passes `None`.
///
/// This is not the scatter #1365 took out. That one built a map of the whole column per column to
/// scatter into. This one writes into the answer's own run and the inverse is worked out once.
///
/// # Errors
///
/// As [`interleave()`], or if `inverse` is given and `order` and `inverse` are not both as long as
/// the pieces, which is the only shape in which one can be the other turned round.
pub fn interleave_placed(
    ty: &LogicalType,
    pieces: &[Vector],
    order: &[usize],
    inverse: Option<&[u32]>,
) -> Result<Vector> {
    let rows: usize = pieces.iter().map(Vector::len).sum();
    if let Some(inverse) = inverse.filter(|inverse| inverse.len() != rows || order.len() != rows) {
        return Err(Error::internal(format!(
            "{} places and {} positions for a permutation of {rows} rows",
            inverse.len(),
            order.len()
        )));
    }
    if let Some(&past) = order.iter().find(|&&index| index >= rows) {
        return Err(Error::internal(format!("row {past} read out of pieces of {rows} rows")));
    }
    if matches!(ty, LogicalType::List(_) | LogicalType::Struct(_) | LogicalType::Map(_, _)) {
        // row at a time: the nested types, for the reason `Assembly::values` gives. They have no run
        // of data to lay end to end and no typed copy to gather with.
        let laid: Vec<Value> = pieces
            .iter()
            .flat_map(|piece| (0..piece.len()).map(|row| piece.value_at(row)))
            .collect();
        let values: Vec<Value> =
            order.iter().map(|&index| laid.get(index).cloned().unwrap_or(Value::Null)).collect();
        return Vector::from_values(ty.clone(), &values);
    }
    if let Some(merged) = merged_dictionary(ty, pieces, order, inverse)? {
        return Ok(merged);
    }
    if let Some(inverse) = inverse {
        if let Some(placed) = placed_strings(ty, pieces, inverse, 0..rows)? {
            return Ok(placed);
        }
        if let Some(placed) = placed_fixed(ty, pieces, inverse)? {
            return Ok(placed);
        }
    }
    let mut data = data_for(ty, rows)?;
    // The untyped null, which has no run of data to lay or to gather out of, and is null whatever
    // the order is.
    if matches!(data, Data::Empty) {
        return Ok(Vector::constant(ty.clone(), Value::Null, order.len()));
    }
    let mut arenas = arenas_of(pieces);
    // Reserved whole, because an arena grown by doubling as the pieces arrive copies what it holds
    // at every step and faults each new allocation in again. The sorted SF1 comments lay 183MB.
    if let Data::Varlen(column) = &mut data {
        column.reserve_bytes(arenas.bytes());
    }
    // Each piece's validity, taken after it is flattened, because a constant null keeps its null in
    // its value rather than in its mask and a flattened one has it in the mask like any other row.
    let mut masks = Vec::with_capacity(pieces.len());
    for piece in pieces {
        // flatten: the gather below reads one run of data, and a piece can arrive dictionary
        // encoded, constant or bit packed. The flatten is a typed loop per layout, a flat piece is
        // not copied by it, and one piece is flattened at a time so a column is never held twice.
        let flat = piece.flatten()?;
        let from = flat.data().ok_or_else(|| Error::internal("a flattened vector with no data"))?;
        let appended = extend(&mut data, from, &mut arenas)?;
        if appended != piece.len() {
            return Err(Error::internal(format!(
                "a piece of {} rows laid {appended} values end to end",
                piece.len()
            )));
        }
        masks.push((flat.len(), flat.validity().clone()));
    }
    let laid = if masks.iter().all(|(_, mask)| matches!(mask, Validity::AllValid)) {
        Validity::AllValid
    } else {
        let mut live = Vec::with_capacity(rows);
        for (len, mask) in &masks {
            // row at a time: a bit a row for the mixed case, once a column rather than once a
            // piece of every column the way it would be read otherwise.
            live.extend((0..*len).map(|row| mask.is_valid(row)));
        }
        Validity::from_run(&live)
    };
    if laid.count_valid(rows) == 0 {
        return Ok(Vector::constant(ty.clone(), Value::Null, order.len()));
    }
    let validity = match (laid, inverse) {
        (Validity::AllValid, _) => Validity::AllValid,
        (laid, Some(inverse)) => {
            let mut live = vec![false; order.len()];
            for (row, &to) in inverse.iter().enumerate() {
                if let Some(slot) = live.get_mut(to as usize) {
                    *slot = laid.is_valid(row);
                }
            }
            Validity::from_run(&live)
        }
        (laid, None) => Validity::from_iter(order.len(), |row| {
            order.get(row).is_some_and(|&index| laid.is_valid(index))
        }),
    };
    if let Data::Varlen(column) = data {
        let (views, arena) = column.into_parts();
        let gathered = match inverse {
            Some(inverse) => {
                let mut placed = vec![StringView::empty(); order.len()];
                for (view, &to) in views.iter().zip(inverse) {
                    if let Some(slot) = placed.get_mut(to as usize) {
                        *slot = *view;
                    }
                }
                placed
            }
            None => order
                .iter()
                .map(|&index| views.get(index).copied().unwrap_or_else(StringView::empty))
                .collect(),
        };
        return Ok(
            Vector::string_views(ty.clone(), gathered, Arc::new(arena))?.with_validity(validity)
        );
    }
    let data = match inverse {
        Some(inverse) => placed_of(&data, inverse),
        None => copy_of(&data, order),
    };
    Ok(Vector::flat(ty.clone(), data)?.with_validity(validity))
}

/// A fixed width column written through `inverse` straight from its pieces, or `None` for a type
/// whose layout is not fixed width.
///
/// The general path lays every piece end to end first and then writes that run through `inverse`,
/// and it flattens every piece that is not flat on the way. On the sorted SF1 `lineitem` those were
/// three passes over every column: the flatten was 7 percent of the busy samples, mostly decoding
/// the dictionary pieces the Parquet reader hands on, the laying was 5 percent more, and the write
/// through `inverse` was 6.5. Here each piece is written to its places as it is read, and a
/// dictionary piece whose values are a flat run with no nulls is written by looking each code up,
/// so a column is read once and written once. A piece in any other form is flattened on its own and
/// then written the same way.
fn placed_fixed(ty: &LogicalType, pieces: &[Vector], inverse: &[u32]) -> Result<Option<Vector>> {
    let rows = inverse.len();
    let mut live: Option<Vec<bool>> = None;
    let mut base = 0;
    // The nulls of one piece written to their places, once a piece with any has arrived.
    let mut mark = |mask: &Validity, places: &[u32]| {
        if matches!(mask, Validity::AllValid) {
            return;
        }
        let live = live.get_or_insert_with(|| vec![true; rows]);
        for (row, &to) in places.iter().enumerate() {
            if let Some(slot) = live.get_mut(to as usize) {
                *slot = mask.is_valid(row);
            }
        }
    };
    macro_rules! placed {
        ($(($variant:ident, $native:ty, $zero:expr)),+ $(,)?) => {
            match data_for(ty, 0)? {
                $(Data::$variant(_) => {
                    let mut out: Vec<$native> = vec![$zero; rows];
                    for piece in pieces {
                        let len = piece.len();
                        let places = inverse.get(base..base + len).ok_or_else(|| {
                            Error::internal("pieces longer than the places they are written to")
                        })?;
                        base += len;
                        let coded = piece.dictionary_parts().and_then(|(codes, values)| {
                            match (values.form(), values.validity(), values.data()) {
                                (Form::Flat, Validity::AllValid, Some(Data::$variant(held))) => {
                                    Some((codes, held.as_slice()))
                                }
                                _ => None,
                            }
                        });
                        if let Some((codes, held)) = coded {
                            for (&code, &to) in codes.iter().zip(places) {
                                if let (Some(slot), Some(value)) =
                                    (out.get_mut(to as usize), held.get(code as usize))
                                {
                                    *slot = *value;
                                }
                            }
                            mark(piece.validity(), places);
                            continue;
                        }
                        // A flat piece is read where it lies.
                        let flat;
                        let piece = if piece.form() == Form::Flat {
                            piece
                        } else {
                            // flatten: a piece that is neither flat nor a dictionary over a flat
                            // run, which a sort's input rarely is.
                            flat = piece.flatten()?;
                            &flat
                        };
                        let Some(Data::$variant(values)) = piece.data() else {
                            return Err(Error::internal(format!(
                                "a piece of {} laid into a column of {ty}",
                                piece.logical_type()
                            )));
                        };
                        if values.len() != len {
                            return Err(Error::internal(format!(
                                "a piece of {len} rows holds {} values",
                                values.len()
                            )));
                        }
                        for (value, &to) in values.iter().zip(places) {
                            if let Some(slot) = out.get_mut(to as usize) {
                                *slot = *value;
                            }
                        }
                        mark(piece.validity(), places);
                    }
                    let validity = match live {
                        None => Validity::AllValid,
                        Some(live) if !live.contains(&true) => {
                            return Ok(Some(Vector::constant(ty.clone(), Value::Null, rows)));
                        }
                        Some(live) => Validity::from_run(&live),
                    };
                    let data = Data::$variant(Buffer::from_vec(out));
                    Ok(Some(Vector::flat(ty.clone(), data)?.with_validity(validity)))
                })+
                _ => Ok(None),
            }
        };
    }
    crate::for_each_layout!(fixed, placed)
}

/// The string column written through `inverse` into an arena laid in the order of the result.
///
/// Laying the pieces' arenas end to end and pushing the views keeps the bytes in the order they
/// arrived, so everything after the sort that reads the strings in their new order reads the arena
/// at random. On the sorted `lineitem` that is `l_comment`, whose distinct count in the append
/// took 550 to 1570 ms of CPU across the threads at full width, nearly all of it waiting on memory,
/// and takes 190 to 250 ms with the arena in order. A table built by the sort also keeps its strings
/// in the order it is read in from then on. Here each piece is read once in order, every long
/// string's length is written to its place first so a prefix sum gives each one its offset, and
/// then its bytes are copied there. A sort's output is a few long runs of its input, so both
/// passes write a few streams that each move forward.
///
/// `None` when the column is not a string, or when a piece is not flat views and has to be
/// flattened on the general path first.
fn placed_strings(
    ty: &LogicalType,
    pieces: &[Vector],
    inverse: &[u32],
    range: Range<usize>,
) -> Result<Option<Vector>> {
    if !strings_placeable(ty, pieces) {
        return Ok(None);
    }
    let first = range.start;
    let rows = range.len();
    // The place of a row in this range, or `None` for a row another range lays.
    let local = |to: u32| (to as usize).checked_sub(first).filter(|&at| at < rows);
    let mut offsets = vec![0u64; rows + 1];
    let mut places = inverse.iter();
    for piece in pieces {
        let (views, _) = piece.text_parts().unwrap_or_default();
        for (view, &to) in views.iter().zip(places.by_ref()) {
            if view.is_inline() {
                continue;
            }
            if let Some(slot) = local(to).and_then(|at| offsets.get_mut(at + 1)) {
                *slot = view.len() as u64;
            }
        }
    }
    let mut total = 0;
    for offset in &mut offsets {
        total += *offset;
        *offset = total;
    }
    let mut arena =
        vec![0u8; usize::try_from(total).map_err(|_| Error::internal("an arena too large"))?];
    let mut placed = vec![StringView::empty(); rows];
    let mut live = vec![true; rows];
    let mut places = inverse.iter();
    for piece in pieces {
        let (views, from) = piece.text_parts().unwrap_or_default();
        let validity = piece.validity();
        for (row, (view, &to)) in views.iter().zip(places.by_ref()).enumerate() {
            let Some(to) = local(to) else {
                continue;
            };
            if !validity.is_valid(row) {
                if let Some(slot) = live.get_mut(to) {
                    *slot = false;
                }
                continue;
            }
            let (Some(bytes), Some(&at), Some(slot)) =
                (view.bytes_in(from), offsets.get(to), placed.get_mut(to))
            else {
                continue;
            };
            if view.is_inline() {
                *slot = *view;
                continue;
            }
            if let Some(into) = arena.get_mut(at as usize..at as usize + bytes.len()) {
                into.copy_from_slice(bytes);
            }
            *slot = StringView::over(bytes, at);
        }
    }
    let validity = if live.iter().all(|&valid| valid) {
        Validity::AllValid
    } else {
        Validity::from_run(&live)
    };
    let vector = Vector::string_views(ty.clone(), placed, Arc::new(Buffer::from_vec(arena)))?;
    Ok(Some(vector.with_validity(validity)))
}

/// Whether [`interleave_placed`] lays this string column through [`placed_string_rows`], which is
/// when it is a string and every piece is flat views.
#[must_use]
pub fn strings_placeable(ty: &LogicalType, pieces: &[Vector]) -> bool {
    matches!(ty, LogicalType::Varchar | LogicalType::Blob)
        && pieces.iter().all(|piece| piece.text_parts().is_some())
}

/// The rows in `range` of the string column [`interleave_placed`] would lay through `inverse`,
/// with an arena of their own.
///
/// This is how a sort builds one long string column on several threads. Each range reads every
/// piece and all of `inverse` and copies only its own strings, so each has a few forward streams
/// to write the way the whole column does, and the ranges share nothing they write.
///
/// # Errors
///
/// If `inverse` is not as long as the pieces or `range` runs past it, or if a piece is not flat
/// views, which [`strings_placeable`] says beforehand.
pub fn placed_string_rows(
    ty: &LogicalType,
    pieces: &[Vector],
    inverse: &[u32],
    range: Range<usize>,
) -> Result<Vector> {
    let rows: usize = pieces.iter().map(Vector::len).sum();
    if inverse.len() != rows || range.end > rows || range.start > range.end {
        return Err(Error::internal(format!(
            "rows {range:?} of {} places for {rows} rows",
            inverse.len()
        )));
    }
    placed_strings(ty, pieces, inverse, range)?
        .ok_or_else(|| Error::internal("a string column placed that is not flat views"))
}

/// How many rows a merged dictionary entry has to stand for on average before a string column is
/// gathered as codes rather than as views.
///
/// A Parquet file carries one dictionary per row group, so a sorted `lineitem` column arrives as
/// 733 pieces over 49 dictionaries. The low cardinality columns have 98 to 343 entries between all
/// of them, and merging those is nothing next to gathering six million views. A column whose
/// dictionaries are nearly as long as the column is one the writer should not have encoded, and
/// merging it would hash every value to save nothing, so it is gathered flat.
const ROWS_PER_MERGED_ENTRY: usize = 8;

/// The string column gathered by `order` as one dictionary, when every piece is a dictionary.
///
/// The pieces' dictionaries are merged into one with each distinct value once, so the codes mean
/// the same thing on every page cut from the result and the result is a stable dictionary. The
/// gather is then four bytes a row instead of sixteen, and the append after the sort gets the
/// dictionary the scan handed up rather than a flat column it has to read a row at a time.
///
/// `None` when the column is not a string, when any piece is not a dictionary or has nulls at its
/// own level, or when the dictionaries are too long for the merge to pay.
fn merged_dictionary(
    ty: &LogicalType,
    pieces: &[Vector],
    order: &[usize],
    inverse: Option<&[u32]>,
) -> Result<Option<Vector>> {
    if !matches!(ty, LogicalType::Varchar | LogicalType::Blob) || pieces.is_empty() {
        return Ok(None);
    }
    let rows: usize = pieces.iter().map(Vector::len).sum();
    let mut dictionaries: Vec<&Arc<Vector>> = Vec::new();
    let mut which = Vec::with_capacity(pieces.len());
    let mut entries = 0;
    for piece in pieces {
        let Some((_, values)) = piece.shared_dictionary_parts() else {
            return Ok(None);
        };
        if !matches!(piece.validity(), Validity::AllValid) {
            return Ok(None);
        }
        let at = match dictionaries.iter().position(|seen| Arc::ptr_eq(seen, values)) {
            Some(at) => at,
            None => {
                entries += values.len();
                if entries.saturating_mul(ROWS_PER_MERGED_ENTRY) > rows {
                    return Ok(None);
                }
                dictionaries.push(values);
                dictionaries.len() - 1
            }
        };
        which.push(at);
    }
    // A null entry has no bytes, so it merges with every other null entry.
    let mut merged: HashMap<Option<&[u8]>, u32> = HashMap::new();
    let mut values = Vec::new();
    let mut remaps = Vec::with_capacity(dictionaries.len());
    for dictionary in &dictionaries {
        let mut remap = Vec::with_capacity(dictionary.len());
        // row at a time: over the dictionary entries, a few hundred of them against millions of
        // rows, and only the first sighting of each value becomes one.
        for entry in 0..dictionary.len() {
            let next = u32::try_from(values.len())
                .map_err(|_| Error::internal("a merged dictionary past four billion entries"))?;
            let code = *merged.entry(dictionary.bytes_at(entry)).or_insert_with(|| {
                values.push(dictionary.value_at(entry));
                next
            });
            remap.push(code);
        }
        remaps.push(remap);
    }
    let mut laid = Vec::with_capacity(rows);
    for (piece, &at) in pieces.iter().zip(&which) {
        let (codes, _) = piece
            .dictionary_parts()
            .ok_or_else(|| Error::internal("a dictionary piece lost its dictionary"))?;
        let remap = &remaps[at];
        laid.extend(codes.iter().map(|&code| remap[code as usize]));
    }
    // Written through the inverse when there is one, for the reason `interleave_placed` gives: a
    // few long runs read through `order` spend a cache line on every four byte code.
    let codes = match inverse {
        Some(inverse) => {
            let mut codes = vec![0u32; order.len()];
            for (&code, &to) in laid.iter().zip(inverse) {
                if let Some(slot) = codes.get_mut(to as usize) {
                    *slot = code;
                }
            }
            codes
        }
        None => order.iter().map(|&index| laid[index]).collect(),
    };
    let values = Vector::from_values(ty.clone(), &values)?;
    Ok(Some(Vector::stable_dictionary(codes, Arc::new(values))?))
}

/// The validity of the pieces laid end to end, in `rows` rows.
///
/// The two cheap answers are checked for first because they are the answers real data gives. A
/// column that was never null anywhere is a page with no mask on it at all, and a bit per row read
/// out of every piece to build a mask that is all ones would be throwing that away.
fn run_of(pieces: &[&Vector], rows: usize) -> Validity {
    if pieces.iter().all(|piece| matches!(piece.validity(), Validity::AllValid)) {
        return Validity::AllValid;
    }
    if pieces.iter().all(|piece| matches!(piece.validity(), Validity::AllInvalid)) {
        return Validity::AllInvalid;
    }
    let mut live = Vec::with_capacity(rows);
    for piece in pieces {
        // row at a time: the mixed case, which is a bit per row however it is written, and it runs
        // once per column per row group rather than once per chunk.
        for row in 0..piece.len() {
            live.push(!piece.is_null_at(row));
        }
    }
    Validity::from_run(&live)
}

/// Whether row `n` reads position `n` for every row, which makes the final gather a copy onto itself.
fn straight(at: &[usize]) -> bool {
    at.iter().enumerate().all(|(row, &index)| row == index)
}

/// The arenas the flat string pieces among `pieces` share, counted before any of them is laid.
fn arenas_of<V: AsRef<Vector>>(pieces: &[V]) -> Arenas {
    let mut arenas = Arenas::default();
    for piece in pieces {
        if let Some(Data::Varlen(column)) = piece.as_ref().data() {
            arenas.count(column);
        }
    }
    arenas
}

/// The pieces as one window, when they are windows of one page that follow each other in it.
///
/// A sorted load is the case. Its answer is laid as one page a column and handed on in chunks cut
/// out of that page, and a table then lays those chunks end to end into row groups, which without
/// this copies every value back into a run the page already holds. A string column joins when its
/// views are a page and every piece shares one arena, which is what a sorted string column is.
fn adjoined(pieces: &[&Vector]) -> Option<Data> {
    macro_rules! joined {
        ($(($variant:ident, $native:ty, $zero:expr)),+ $(,)?) => {
            match pieces.first()?.data()? {
                $(Data::$variant(first) => {
                    if !first.is_shared() {
                        return None;
                    }
                    let mut run = first.clone();
                    for piece in &pieces[1..] {
                        let Some(Data::$variant(next)) = piece.data() else { return None };
                        run = run.joined(next)?;
                    }
                    Some(Data::$variant(run))
                })+
                Data::Varlen(first) => {
                    if !first.is_paged() {
                        return None;
                    }
                    let mut run = first.clone();
                    for piece in &pieces[1..] {
                        let Some(Data::Varlen(next)) = piece.data() else { return None };
                        run = run.joined(next)?;
                    }
                    Some(Data::Varlen(run))
                }
                _ => None,
            }
        };
    }
    crate::for_each_layout!(fixed, joined)
}

/// Lays a run of data end to end after another, answering how many values it appended.
///
/// The typed loop per layout is the whole point: an append of a thousand `i64` is one `memcpy` and
/// an append of a thousand strings is at most one copy of an arena and a thousand sixteen byte views,
/// neither of which touches a `Value`. `arenas` is what says whether the arena is copied whole.
fn extend(into: &mut Data, from: &Data, arenas: &mut Arenas) -> Result<usize> {
    macro_rules! extended {
        ($(($variant:ident, $native:ty, $zero:expr)),+ $(,)?) => {
            match (&mut *into, from) {
                // Nothing to append, which is what an untyped null piece is. The caller reads the
                // count and leaves those rows null rather than pointing them anywhere.
                (_, Data::Empty) => Ok(0),
                $((Data::$variant(out), Data::$variant(values)) => {
                    out.extend_from_slice(values.as_slice());
                    Ok(values.len())
                })+
                // The one layout where an append is a copy of bytes rather than a copy of fixed
                // width slots, and the column decides whether that is one copy or one a string.
                (Data::Varlen(out), Data::Varlen(values)) => {
                    out.push_column(values, arenas);
                    Ok(values.len())
                }
                (out, from) => Err(Error::internal(format!(
                    "a run of {:?} values cannot be laid after a run of {:?} ones",
                    layout_of(from),
                    layout_of(out)
                ))),
            }
        };
    }
    crate::for_each_layout!(fixed, extended)
}

/// Unit tests for the assembly.
///
/// That the engine actually goes through here rather than through the old path was checked rather
/// than assumed, by gating a panic on [`Assembly::place`] and running the `rudb` suite with it
/// armed. Three tests failed and no others: the one that is a `CASE` by name, the one that runs the
/// catalog views the engine ships with, and the one over the native frequency synopsis. All three
/// have a `CASE` in them and nothing else in the suite does.
/// The picks as codes into the one stable dictionary every source shares, or `None` when they do
/// not all share one. A pick that names no source, or a row that is null, is null.
fn picked_codes(sources: &[&Vector], picks: &[(u32, u32)]) -> Result<Option<Vector>> {
    let mut shared: Option<&Arc<Vector>> = None;
    let mut runs = Vec::with_capacity(sources.len());
    for source in sources {
        let Some((codes, values)) = source.stable_dictionary_parts() else { return Ok(None) };
        if shared.is_some_and(|held| !Arc::ptr_eq(held, values)) {
            return Ok(None);
        }
        shared = Some(values);
        runs.push(codes);
    }
    // A dictionary with no values has no code to stand in for a null row.
    let Some(values) = shared.filter(|values| !values.is_empty()) else { return Ok(None) };
    let mut live = Vec::with_capacity(picks.len());
    let mut codes = Vec::with_capacity(picks.len());
    for &(source, row) in picks {
        let code = sources.get(source as usize).and_then(|held| {
            let row = row as usize;
            let code = runs[source as usize].get(row)?;
            held.validity().is_valid(row).then_some(*code)
        });
        live.push(code.is_some());
        codes.push(code.unwrap_or(0));
    }
    let vector = Vector::stable_dictionary(codes, Arc::clone(values))?;
    Ok(Some(if live.iter().all(|&alive| alive) {
        vector
    } else {
        vector.with_validity(Validity::from_run(&live))
    }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{Chunk, Form};

    /// Every row of a vector, as values, which is what an assembly is checked against.
    fn values(vector: &Vector) -> Vec<Value> {
        (0..vector.len()).map(|row| vector.value_at(row)).collect()
    }

    /// The answer the assembly has to reach, written the slow obvious way.
    ///
    /// A `Vec<Value>` filled by scattering and then handed to [`Vector::from_values`] is exactly
    /// what `CASE` used to do, so this is the reference rather than a second opinion.
    fn scattered(ty: &LogicalType, rows: usize, pieces: &[(Vec<u32>, Vector)]) -> Vector {
        let mut answers = vec![Value::Null; rows];
        for (positions, piece) in pieces {
            for (slot, &row) in positions.iter().enumerate() {
                answers[row as usize] = piece.value_at(slot);
            }
        }
        Vector::from_values(ty.clone(), &answers).expect("the reference builds")
    }

    /// Builds an assembly out of pieces and checks it against the slow way of getting there.
    fn agrees(ty: &LogicalType, rows: usize, pieces: &[(Vec<u32>, Vector)]) -> Vector {
        let mut assembly = Assembly::new(ty.clone(), rows).expect("an assembly of this type");
        for (positions, piece) in pieces {
            assembly.place(positions, piece).expect("the piece is placed");
        }
        let built = assembly.finish().expect("the assembly finishes");
        assert_eq!(built.len(), rows, "an assembly of {rows} rows");
        assert_eq!(values(&built), values(&scattered(ty, rows, pieces)), "against the slow way");
        built
    }

    #[test]
    fn two_pieces_interleave_back_into_the_order_the_rows_came_in() {
        let evens = Vector::from_values(LogicalType::BigInt, &[Value::BigInt(0), Value::BigInt(2)])
            .expect("a vector");
        let odds = Vector::from_values(LogicalType::BigInt, &[Value::BigInt(1), Value::BigInt(3)])
            .expect("a vector");
        let built = agrees(&LogicalType::BigInt, 4, &[(vec![0, 2], evens), (vec![1, 3], odds)]);
        assert_eq!(
            values(&built),
            vec![Value::BigInt(0), Value::BigInt(1), Value::BigInt(2), Value::BigInt(3)]
        );
    }

    #[test]
    fn a_row_no_piece_claims_is_null() {
        // Which is what a `CASE` with no `ELSE` leaves behind, and the case where a run of data with
        // a hole in it would put every value after the hole at the wrong index.
        let piece =
            Vector::from_values(LogicalType::BigInt, &[Value::BigInt(7)]).expect("a vector");
        let built = agrees(&LogicalType::BigInt, 3, &[(vec![1], piece)]);
        assert_eq!(values(&built), vec![Value::Null, Value::BigInt(7), Value::Null]);
    }

    #[test]
    fn no_pieces_at_all_is_a_column_of_nulls_of_the_right_length() {
        let built = agrees(&LogicalType::Integer, 5, &[]);
        assert!(built.is_null_at(4), "every row of it is null");
    }

    #[test]
    fn a_null_inside_a_piece_stays_null_where_the_piece_put_it() {
        // The validity has to survive the copy, and the value under it has to not be read, which is
        // two different things a single run of data with a mask over it can get wrong separately.
        let piece = Vector::from_values(
            LogicalType::BigInt,
            &[Value::BigInt(1), Value::Null, Value::BigInt(3)],
        )
        .expect("a vector");
        let built = agrees(&LogicalType::BigInt, 3, &[(vec![2, 0, 1], piece)]);
        assert!(built.is_null_at(0), "the null landed where the piece put it");
        assert_eq!(built.value_at(2), Value::BigInt(1));
    }

    #[test]
    fn strings_are_assembled_without_going_through_a_value_each() {
        let left = Vector::from_values(
            LogicalType::Varchar,
            &[Value::Varchar("a short one".into()), Value::Varchar("another".into())],
        )
        .expect("a vector");
        let right = Vector::from_values(
            LogicalType::Varchar,
            &[Value::Varchar("a string that is far too long to live inline in a view".into())],
        )
        .expect("a vector");
        let built = agrees(&LogicalType::Varchar, 3, &[(vec![0, 2], left), (vec![1], right)]);
        assert_eq!(built.value_at(0), Value::Varchar("a short one".into()));
        assert_eq!(
            built.value_at(1),
            Value::Varchar("a string that is far too long to live inline in a view".into())
        );
        assert_eq!(built.value_at(2), Value::Varchar("another".into()));
    }

    #[test]
    fn strings_laid_end_to_end_in_order_come_back_as_views_over_the_arena_they_went_into() {
        // The shape a hash join's gathered side is: chunk after chunk, each claiming the rows
        // straight after the last, so the permutation is the identity and nothing needs moving.
        let first = Vector::from_values(
            LogicalType::Varchar,
            &[Value::Varchar("one".into()), Value::Varchar("two".into())],
        )
        .expect("a vector");
        let second = Vector::from_values(
            LogicalType::Varchar,
            &[Value::Varchar("a third one long enough to be out of line".into())],
        )
        .expect("a vector");
        let built = agrees(&LogicalType::Varchar, 3, &[(vec![0, 1], first), (vec![2], second)]);
        assert_eq!(built.form(), Form::StringView, "the bytes stay where they were appended");
        assert_eq!(
            built.value_at(2),
            Value::Varchar("a third one long enough to be out of line".into())
        );
    }

    #[test]
    fn a_string_row_no_piece_claims_is_null_rather_than_empty() {
        // The hole a `CASE` with no `ELSE` leaves, on the path that permutes views instead of
        // copying bytes, where an unclaimed row has no view to read and has to come out null.
        let piece = Vector::from_values(
            LogicalType::Varchar,
            &[Value::Varchar("a value long enough to be out of line".into())],
        )
        .expect("a vector");
        let built = agrees(&LogicalType::Varchar, 3, &[(vec![2], piece)]);
        assert_eq!(built.value_at(0), Value::Null);
        assert_eq!(built.value_at(1), Value::Null);
        assert_eq!(
            built.value_at(2),
            Value::Varchar("a value long enough to be out of line".into())
        );
    }

    #[test]
    fn a_constant_piece_is_written_out_rather_than_read_a_row_at_a_time() {
        // The `ELSE ''` half of the ClickBench query this was built for, which arrives as a constant
        // over however many rows the arms did not claim.
        let arm = Vector::from_values(LogicalType::Varchar, &[Value::Varchar("kept".into())])
            .expect("a vector");
        let otherwise = Vector::constant(LogicalType::Varchar, Value::Varchar("".into()), 3);
        let built = agrees(&LogicalType::Varchar, 4, &[(vec![2], arm), (vec![0, 1, 3], otherwise)]);
        assert_eq!(built.value_at(0), Value::Varchar("".into()));
        assert_eq!(built.value_at(2), Value::Varchar("kept".into()));
    }

    #[test]
    fn a_dictionary_piece_is_walked_to_its_values() {
        // A scanned string column arrives as codes over a shared dictionary, so this is the form the
        // `THEN Referer` arm of the ClickBench query actually hands over.
        let dictionary = Vector::from_values(
            LogicalType::Varchar,
            &[Value::Varchar("one".into()), Value::Varchar("two".into())],
        )
        .expect("a dictionary");
        let piece = Vector::dictionary(vec![1, 0, 1], dictionary).expect("a dictionary vector");
        let built = agrees(&LogicalType::Varchar, 3, &[(vec![0, 1, 2], piece)]);
        assert_eq!(
            values(&built),
            vec![
                Value::Varchar("two".into()),
                Value::Varchar("one".into()),
                Value::Varchar("two".into())
            ]
        );
    }

    #[test]
    fn a_piece_placed_at_the_wrong_number_of_positions_is_an_error() {
        let piece =
            Vector::from_values(LogicalType::BigInt, &[Value::BigInt(1)]).expect("a vector");
        let mut assembly = Assembly::new(LogicalType::BigInt, 4).expect("an assembly");
        assert!(assembly.place(&[0, 1], &piece).is_err(), "two positions for one row");
    }

    #[test]
    fn a_position_past_the_end_is_an_error_rather_than_a_lost_row() {
        let piece =
            Vector::from_values(LogicalType::BigInt, &[Value::BigInt(1)]).expect("a vector");
        let mut assembly = Assembly::new(LogicalType::BigInt, 2).expect("an assembly");
        assert!(assembly.place(&[9], &piece).is_err(), "a row past the end of the assembly");
    }

    #[test]
    fn a_piece_of_the_wrong_layout_is_an_error_rather_than_a_wrong_answer() {
        // Two runs of data that cannot be laid end to end, which is a bug in whoever built the
        // pieces and has to say so rather than silently keep the first one.
        let piece =
            Vector::from_values(LogicalType::Varchar, &[Value::Varchar("x".into())]).expect("text");
        let mut assembly = Assembly::new(LogicalType::BigInt, 1).expect("an assembly");
        assert!(assembly.place(&[0], &piece).is_err(), "text laid after integers");
    }

    #[test]
    fn every_layout_assembles_the_way_it_scatters() {
        // One case per physical layout, because the copy loop is generated per layout and a layout
        // missing from it is a wrong answer for that type alone, which no single typed test finds.
        let cases: Vec<(LogicalType, Vec<Value>)> = vec![
            (LogicalType::Boolean, vec![Value::Boolean(true), Value::Boolean(false)]),
            (LogicalType::TinyInt, vec![Value::TinyInt(1), Value::TinyInt(-2)]),
            (LogicalType::SmallInt, vec![Value::SmallInt(3), Value::SmallInt(-4)]),
            (LogicalType::Integer, vec![Value::Integer(5), Value::Integer(-6)]),
            (LogicalType::BigInt, vec![Value::BigInt(7), Value::BigInt(-8)]),
            (LogicalType::HugeInt, vec![Value::HugeInt(9), Value::HugeInt(-10)]),
            (LogicalType::UTinyInt, vec![Value::UTinyInt(11), Value::UTinyInt(12)]),
            (LogicalType::USmallInt, vec![Value::USmallInt(13), Value::USmallInt(14)]),
            (LogicalType::UInteger, vec![Value::UInteger(15), Value::UInteger(16)]),
            (LogicalType::UBigInt, vec![Value::UBigInt(17), Value::UBigInt(18)]),
            (LogicalType::Float, vec![Value::Float(1.5), Value::Float(-2.5)]),
            (LogicalType::Double, vec![Value::Double(3.5), Value::Double(-4.5)]),
            (
                LogicalType::Varchar,
                vec![Value::Varchar("first".into()), Value::Varchar("second".into())],
            ),
            (LogicalType::Date, vec![Value::Date(19), Value::Date(20)]),
        ];
        for (ty, pair) in cases {
            let left = Vector::from_values(ty.clone(), &pair[..1]).expect("a vector");
            let right = Vector::from_values(ty.clone(), &pair[1..]).expect("a vector");
            let built = agrees(&ty, 2, &[(vec![1], left), (vec![0], right)]);
            assert_eq!(built.value_at(0), pair[1], "{ty:?} at row 0");
            assert_eq!(built.value_at(1), pair[0], "{ty:?} at row 1");
        }
    }

    #[test]
    fn an_assembly_is_a_chunk_column_like_any_other() {
        // The point of building a vector rather than a `Vec<Value>` is that what comes out goes
        // straight into a chunk, so this checks it actually does.
        let piece = Vector::from_values(LogicalType::BigInt, &[Value::BigInt(1), Value::BigInt(2)])
            .expect("a vector");
        let built = agrees(&LogicalType::BigInt, 2, &[(vec![1, 0], piece)]);
        let chunk = Chunk::new(vec![built]).expect("a chunk of one column");
        assert_eq!(chunk.len(), 2, "two rows");
    }

    /// A run of pieces, as values, in the order they were given.
    fn all_of(pieces: &[Vector]) -> Vec<Value> {
        pieces.iter().flat_map(values).collect()
    }

    /// The pieces laid end to end, checked against the values that went in.
    fn laid(ty: &LogicalType, pieces: &[Vector]) -> Vector {
        let built = concat(ty, pieces).expect("the pieces lay").expect("this run lays");
        assert_eq!(built.len(), pieces.iter().map(Vector::len).sum::<usize>(), "the row count");
        assert_eq!(values(&built), all_of(pieces), "the values laid end to end");
        built
    }

    #[test]
    fn pieces_laid_end_to_end_read_back_in_the_order_they_were_given() {
        let piece = |from: i64, to: i64| {
            let held: Vec<Value> = (from..to).map(Value::BigInt).collect();
            Vector::from_values(LogicalType::BigInt, &held).expect("a run of bigints")
        };
        let pieces = [piece(0, 4), piece(4, 9), piece(9, 10)];
        let built = laid(&LogicalType::BigInt, &pieces);
        assert_eq!(built.form(), Form::Flat, "a run of flat pieces lays flat");
        // The point of the page: a window cut out of it is a reference count bump and not a copy,
        // which is what the table cuts a chunk with.
        let window = built.slice(4, 5).expect("a window into the page");
        assert_eq!(values(&window), all_of(&pieces[1..2]), "the second piece, cut back out");
    }

    /// Windows of one page that follow each other lay as one window over it, and anything else
    /// still lays by copying, which is what a sorted load hands a table.
    #[test]
    fn neighbouring_windows_of_one_page_lay_without_a_copy() {
        let held: Vec<Value> =
            (0..20).map(|at| if at % 7 == 3 { Value::Null } else { Value::BigInt(at) }).collect();
        let page = Vector::from_values(LogicalType::BigInt, &held).expect("a run").into_pages();
        let cut = |from: usize, len: usize| page.slice(from, len).expect("a window");
        let address = |vector: &Vector| match vector.data() {
            Some(Data::Int64(run)) => run.as_slice().as_ptr() as usize,
            other => panic!("a bigint run laid as {other:?}"),
        };
        let built = laid(&LogicalType::BigInt, &[cut(2, 5), cut(7, 8), cut(15, 3)]);
        assert_eq!(address(&built), address(&page) + 2 * 8, "the neighbours were copied");
        // A gap, a piece out of order, and a piece of another page all fall back to the copy.
        let other = Vector::from_values(LogicalType::BigInt, &held).expect("a run").into_pages();
        let other_cut = other.slice(7, 3).expect("a window");
        for pieces in [
            vec![cut(2, 5), cut(8, 3)],
            vec![cut(7, 3), cut(2, 5)],
            vec![cut(2, 5), other_cut],
            vec![Vector::from_values(LogicalType::BigInt, &held[..4]).expect("owned"), cut(4, 2)],
        ] {
            let built = laid(&LogicalType::BigInt, &pieces);
            assert_ne!(address(&built), address(&page) + 2 * 8, "a copy was expected");
        }
    }

    /// The same for strings: cuts of one paged column lay back as a window over its views and
    /// its arena, and a cut of a column whose views are its own is copied.
    #[test]
    fn neighbouring_cuts_of_a_paged_string_column_lay_without_a_copy() {
        let held: Vec<Value> = (0..20)
            .map(|at| Value::Varchar(format!("a string long enough for the arena {at}")))
            .collect();
        let page = Vector::from_values(LogicalType::Varchar, &held).expect("a run").into_pages();
        let cut = |from: usize, len: usize| page.slice(from, len).expect("a window");
        let views = |vector: &Vector| match vector.data() {
            Some(Data::Varlen(column)) => column.views().as_ptr() as usize,
            other => panic!("a varchar run laid as {other:?}"),
        };
        let built = laid(&LogicalType::Varchar, &[cut(2, 5), cut(7, 8), cut(15, 3)]);
        assert_eq!(views(&built), views(&page) + 2 * size_of::<StringView>(), "views copied");
        let owned = Vector::from_values(LogicalType::Varchar, &held).expect("a run");
        let copied = laid(&LogicalType::Varchar, &[owned.slice(0, 4).expect("a cut"), cut(4, 2)]);
        assert_eq!(copied.len(), 6);
    }

    #[test]
    fn a_null_in_a_piece_is_a_null_in_the_same_row_of_the_page() {
        let ty = LogicalType::Integer;
        let whole = Vector::from_values(ty.clone(), &[Value::Integer(1), Value::Integer(2)])
            .expect("no nulls");
        let holed =
            Vector::from_values(ty.clone(), &[Value::Null, Value::Integer(4)]).expect("one null");
        let built = laid(&ty, &[whole.clone(), holed.clone()]);
        assert!(!built.is_null_at(1), "a row that was not null became one");
        assert!(built.is_null_at(2), "the null did not come through");
        // A run with no null anywhere keeps the cheap answer rather than growing a mask of ones.
        let clean = laid(&ty, &[whole.clone(), whole]);
        assert_eq!(clean.validity(), &Validity::AllValid, "a mask nothing needed");
        let empty = laid(&ty, &[holed.clone(), holed]);
        assert!(empty.is_null_at(0) && empty.is_null_at(2), "both nulls came through");
    }

    /// The string case, which is the one that would be a byte copy per cut if it laid flat.
    #[test]
    fn strings_lay_into_one_arena_and_come_back_as_views() {
        let ty = LogicalType::Varchar;
        let word = |text: &str| {
            Vector::from_values(ty.clone(), &[Value::Varchar(text.to_string())]).expect("a string")
        };
        let pieces = [word("a string too long to sit inside a view"), word("short")];
        let built = laid(&ty, &pieces);
        assert_eq!(
            built.form(),
            Form::StringView,
            "a varchar page that is not views cuts by copying"
        );
        let window = built.slice(0, 1).expect("a window into the page");
        assert_eq!(values(&window), all_of(&pieces[..1]), "the long string, cut back out");
    }

    #[test]
    fn stable_dictionary_pieces_sharing_values_lay_as_codes() {
        let ty = LogicalType::Varchar;
        let values = Arc::new(
            Vector::from_values(
                ty.clone(),
                &[Value::Varchar("a".to_string()), Value::Varchar("b".to_string())],
            )
            .expect("dictionary values"),
        );
        let first = Vector::stable_dictionary(vec![1, 0], Arc::clone(&values)).expect("codes");
        let second = Vector::stable_dictionary(vec![1], Arc::clone(&values)).expect("codes");
        let built = concat(&ty, &[first, second]).expect("no error").expect("shared codes lay");
        let (codes, held) = built.stable_dictionary_parts().expect("the stable form survives");
        assert_eq!(codes, &[1, 0, 1]);
        assert!(Arc::ptr_eq(held, &values));
    }

    /// What will not lay, which is a layout answer and not an error.
    #[test]
    fn an_encoded_piece_is_left_alone_rather_than_flattened() {
        let ty = LogicalType::BigInt;
        let flat = Vector::from_values(ty.clone(), &[Value::BigInt(1)]).expect("a flat piece");
        let values = Vector::from_values(ty.clone(), &[Value::BigInt(7), Value::BigInt(8)])
            .expect("two distinct values");
        let coded = Vector::dictionary(vec![0, 1, 0], values).expect("a dictionary piece");
        let one = std::slice::from_ref(&coded);
        assert!(concat(&ty, one).expect("no error").is_none(), "a dictionary laid");
        assert!(
            concat(&ty, &[flat.clone(), coded]).expect("no error").is_none(),
            "a mixed run laid"
        );
        assert!(
            concat::<Vector>(&ty, &[]).expect("no error").is_none(),
            "nothing laid into something"
        );
        // A piece of another type is the caller's mistake and is still answered as a layout it will
        // not build, because the fallback keeps the pieces and keeping them is always correct.
        let other =
            Vector::from_values(LogicalType::Integer, &[Value::Integer(1)]).expect("an int");
        assert!(concat(&ty, &[flat, other]).expect("no error").is_none(), "two types laid");
    }

    /// A [`Spread`] that runs each piece on a thread of its own, in no particular order.
    ///
    /// Not what the engine passes, which shares the pieces out over a fixed lease off a counter. This
    /// is the harsher version on purpose: a thread per piece and nothing deciding who goes first is
    /// the widest the interleaving can get, so anything in the copy that depends on piece order
    /// happening to be arrival order shows up here.
    fn on_a_thread_each(count: usize, task: &(dyn Fn(usize) + Sync)) -> Result<()> {
        std::thread::scope(|scope| {
            let running: Vec<_> =
                (0..count).rev().map(|at| scope.spawn(move || task(at))).collect();
            for thread in running {
                thread.join().expect("a piece copier panicked");
            }
        });
        Ok(())
    }

    /// A run of string pieces that each own an arena, which is what the parts of a stored column are.
    ///
    /// The strings are past the inline limit on purpose. A column of short strings has no arena worth
    /// copying and would pass the same test without the offsets ever being exercised.
    fn owned_strings(pieces: usize, each: usize) -> Vec<Vector> {
        (0..pieces)
            .map(|piece| {
                let held: Vec<Value> = (0..each)
                    .map(|row| match (piece + row) % 4 {
                        0 => Value::Null,
                        1 => Value::Varchar(format!("short {row}")),
                        _ => Value::Varchar(format!(
                            "a string of piece {piece} row {row} that is well past twelve bytes"
                        )),
                    })
                    .collect();
                Vector::from_values(LogicalType::Varchar, &held).expect("a run of strings")
            })
            .collect()
    }

    #[test]
    fn string_pieces_laid_on_many_threads_hold_the_same_strings_as_laid_on_one() {
        let ty = LogicalType::Varchar;
        let pieces = owned_strings(9, 7);
        // Asserted rather than assumed. Every case below agrees with the serial path whichever path
        // ran, so a test that only compared values would still pass if the shape check quietly
        // stopped taking anything, and it is the shape check that this whole file turns on.
        let borrowed: Vec<&Vector> = pieces.iter().collect();
        assert!(apart(&ty, &borrowed).is_some(), "the parallel lay declined its own case");
        let serial = concat(&ty, &pieces).expect("no error").expect("owned arenas lay");
        let parallel = concat_on(&ty, &pieces, &on_a_thread_each)
            .expect("no error")
            .expect("owned arenas lay");
        assert_eq!(parallel.len(), serial.len(), "the row count");
        assert_eq!(values(&parallel), all_of(&pieces), "the values laid end to end");
        assert_eq!(values(&parallel), values(&serial), "the two paths disagree");
        // The form matters as much as the values. A gather off this is a gather off one page of views,
        // and the serial path ends in the same place, so a caller cannot tell which one ran.
        assert_eq!(parallel.form(), serial.form(), "a different body came out");
    }

    /// The shapes the parallel path hands back, each of which is a case the serial one does better.
    #[test]
    fn a_run_the_parallel_lay_does_not_own_is_left_to_the_serial_one() {
        let ty = LogicalType::Varchar;
        let pieces = owned_strings(3, 5);
        assert!(apart(&ty, &[&pieces[0]]).is_none(), "one piece was taken");
        // Cuts of one page share an arena, and the serial path copies it once and shifts the views of
        // every cut. Copying it per cut here would hold it three times over.
        let page = concat(&ty, &pieces).expect("no error").expect("a page").into_pages();
        let cut = |from: usize, len: usize| page.slice(from, len).expect("a window");
        let cuts = [cut(0, 4), cut(4, 6), cut(10, 5)];
        let borrowed: Vec<&Vector> = cuts.iter().collect();
        assert!(apart(&ty, &borrowed).is_none(), "cuts of one page were taken");
        // And a run that goes down the shared view path answers the same either way, which is the
        // thing the fall through is there to preserve.
        let serial = concat(&ty, &cuts).expect("no error").expect("shared views lay");
        let parallel =
            concat_on(&ty, &cuts, &on_a_thread_each).expect("no error").expect("shared views");
        assert_eq!(values(&parallel), values(&serial), "the fall through changed the answer");
    }

    /// A piece whose arena holds bytes nobody reads is the filtered cut of a Parquet page, and taking
    /// it would carry the rest of the page along for as long as the result lives.
    #[test]
    fn a_piece_holding_more_arena_than_it_reads_is_left_to_the_serial_lay() {
        let ty = LogicalType::Varchar;
        let pieces = owned_strings(2, 8);
        let page = concat(&ty, &pieces).expect("no error").expect("a page");
        // One row out of sixteen, so the arena is far larger than the one string read out of it. The
        // slice keeps the whole arena, which is exactly the case being asked about.
        let thin = page.slice(2, 1).expect("a window").flatten().expect("flattened");
        let fat = page.slice(3, 1).expect("a window").flatten().expect("flattened");
        let held = [thin, fat];
        let borrowed: Vec<&Vector> = held.iter().collect();
        if borrowed.iter().all(|piece| match piece.data() {
            Some(Data::Varlen(column)) => !column.mostly_read(),
            _ => false,
        }) {
            assert!(apart(&ty, &borrowed).is_none(), "a mostly unread arena was taken");
        }
        let serial = concat(&ty, &held).expect("no error").expect("flat pieces lay");
        let parallel =
            concat_on(&ty, &held, &on_a_thread_each).expect("no error").expect("flat pieces");
        assert_eq!(values(&parallel), values(&serial), "the two paths disagree");
    }

    /// Pieces of every form a sort hands over, read back through an order, against the same order
    /// read a value at a time.
    #[test]
    fn an_interleave_reads_the_pieces_in_the_order_it_is_given() {
        let words: Vec<Value> = ["a long enough word to leave the inline view", "b", "c"]
            .iter()
            .map(|word| Value::Varchar((*word).to_string()))
            .collect();
        let dictionary = Vector::from_values(LogicalType::Varchar, &words).expect("words");
        let strings = [
            Vector::dictionary(vec![2, 0, 1], dictionary).expect("a dictionary"),
            Vector::from_values(
                LogicalType::Varchar,
                &[Value::Null, Value::Varchar("another string past twelve bytes".to_string())],
            )
            .expect("flat"),
        ];
        let numbers = [
            Vector::from_values(
                LogicalType::BigInt,
                &[Value::BigInt(7), Value::Null, Value::BigInt(9)],
            )
            .expect("flat"),
            Vector::constant(LogicalType::BigInt, Value::BigInt(4), 1),
            Vector::constant(LogicalType::BigInt, Value::Null, 1),
        ];
        let lists = [
            Vector::from_values(
                LogicalType::List(Box::new(LogicalType::Integer)),
                &[
                    Value::List { element: LogicalType::Integer, values: vec![Value::Integer(1)] },
                    Value::Null,
                    Value::List { element: LogicalType::Integer, values: vec![] },
                ],
            )
            .expect("lists"),
            Vector::from_values(
                LogicalType::List(Box::new(LogicalType::Integer)),
                &[
                    Value::List {
                        element: LogicalType::Integer,
                        values: vec![Value::Integer(2), Value::Integer(3)],
                    },
                    Value::Null,
                ],
            )
            .expect("lists"),
        ];
        let order = [4, 0, 3, 1, 2, 3];
        for pieces in [&strings[..], &numbers[..], &lists[..]] {
            let ty = pieces[0].logical_type().clone();
            let laid: Vec<Value> = pieces.iter().flat_map(values).collect();
            let expected: Vec<Value> = order.iter().map(|&index| laid[index].clone()).collect();
            let got = interleave(&ty, pieces, &order).expect("an interleave");
            assert_eq!(values(&got), expected, "{ty}");
        }
        assert!(interleave(&LogicalType::BigInt, &numbers, &[5]).is_err(), "row 5 of 5 rows");
        // The same answers written through the inverse of a permutation, which is the way round a
        // sort takes when its order is a few long runs.
        let order = [4, 0, 3, 1, 2];
        let mut inverse = [0u32; 5];
        for (at, &row) in order.iter().enumerate() {
            inverse[row] = at as u32;
        }
        let texts: Vec<Value> = ["a string past the twelve bytes of a view", "short", "x"]
            .iter()
            .map(|text| Value::Varchar((*text).to_string()))
            .chain([Value::Varchar("another long string for the arena".to_string())])
            .collect();
        let valid = [
            Vector::from_values(LogicalType::Varchar, &texts[..2]).expect("flat"),
            Vector::from_values(LogicalType::Varchar, &texts[2..]).expect("flat"),
            Vector::constant(LogicalType::Varchar, Value::Varchar("one more".to_string()), 1),
        ];
        for pieces in [&strings[..], &numbers[..], &lists[..], &valid[..]] {
            let ty = pieces[0].logical_type().clone();
            let pulled = interleave(&ty, pieces, &order).expect("an interleave");
            let pushed =
                interleave_placed(&ty, pieces, &order, Some(&inverse)).expect("a placed one");
            assert_eq!(values(&pushed), values(&pulled), "{ty}");
        }
        assert!(
            interleave_placed(&LogicalType::BigInt, &numbers, &order, Some(&inverse[..4])).is_err(),
            "four places for five rows"
        );
        let untyped = [Vector::constant(LogicalType::Null, Value::Null, 3)];
        let got = interleave(&LogicalType::Null, &untyped, &[2, 0]).expect("an untyped null");
        assert_eq!(values(&got), vec![Value::Null, Value::Null]);
    }

    /// A fixed width column written through `inverse` from pieces of every form the sort sees,
    /// against the answer read through `order`, and a column of nothing but nulls.
    #[test]
    fn a_fixed_width_column_is_written_to_its_places_from_pieces_of_any_form() {
        let ty = LogicalType::BigInt;
        let int = Value::BigInt;
        let flat = Vector::from_values(ty.clone(), &[int(1), Value::Null, int(3)]).expect("flat");
        let paged = Vector::from_values(ty.clone(), &[int(4), int(5)]).expect("flat").into_pages();
        let words = Vector::from_values(ty.clone(), &[int(70), int(80)]).expect("values");
        let coded = Vector::dictionary(vec![1, 0, 1], words).expect("coded");
        let nulled = Vector::from_values(ty.clone(), &[int(90), Value::Null]).expect("values");
        let chained = Vector::dictionary(vec![1, 0], nulled).expect("coded over nulls");
        let constant = Vector::constant(ty.clone(), int(6), 2);
        let pieces = [flat, paged, coded, chained, constant];
        let rows: usize = pieces.iter().map(Vector::len).sum();
        let order: Vec<usize> = (0..rows).map(|at| (at * 5 + 3) % rows).collect();
        let mut inverse = vec![0u32; rows];
        for (to, &from) in order.iter().enumerate() {
            inverse[from] = u32::try_from(to).expect("a small row");
        }
        let read = interleave_placed(&ty, &pieces, &order, None).expect("read through order");
        let written =
            interleave_placed(&ty, &pieces, &order, Some(&inverse)).expect("written to places");
        assert_eq!(values(&written), values(&read));
        assert_eq!(values(&written)[inverse[1] as usize], Value::Null, "the flat piece's null");
        assert_eq!(values(&written)[inverse[8] as usize], Value::Null, "the dictionary's null");

        let nothing = Vector::from_values(ty.clone(), &[Value::Null, Value::Null]).expect("nulls");
        let written = interleave_placed(&ty, &[nothing], &[1, 0], Some(&[1, 0])).expect("nulls");
        assert_eq!(values(&written), [Value::Null, Value::Null]);
    }

    #[test]
    fn placed_strings_are_laid_in_the_order_of_the_result() {
        let word = |text: &str| Value::Varchar(text.to_string());
        let flat = Vector::from_values(
            LogicalType::Varchar,
            &[word("the first string past twelve bytes"), Value::Null, word("short")],
        )
        .expect("flat");
        let arena = b"xxa second string past twelve bytesyy".to_vec();
        let views = vec![StringView::over(&arena[2..35], 2), StringView::inline("tiny")];
        let viewed =
            Vector::string_views(LogicalType::Varchar, views, Arc::new(Buffer::from_vec(arena)))
                .expect("views");
        let pieces = [flat, viewed];
        let order = [3, 0, 4, 2, 1];
        let mut inverse = vec![0u32; order.len()];
        for (to, &from) in order.iter().enumerate() {
            inverse[from] = u32::try_from(to).expect("a small row");
        }
        let laid: Vec<Value> = pieces.iter().flat_map(values).collect();
        let expected: Vec<Value> = order.iter().map(|&index| laid[index].clone()).collect();
        let got = interleave_placed(&LogicalType::Varchar, &pieces, &order, Some(&inverse))
            .expect("a placed interleave");
        assert_eq!(values(&got), expected);
        let (_, arena) = got.text_parts().expect("views");
        assert_eq!(
            arena, b"a second string past twelve bytesthe first string past twelve bytes",
            "the long strings in the order they come out, and nothing else"
        );
        assert!(strings_placeable(&LogicalType::Varchar, &pieces));
        for split in 0..=order.len() {
            let mut joined = Vec::new();
            for range in [0..split, split..order.len()] {
                let part = placed_string_rows(&LogicalType::Varchar, &pieces, &inverse, range)
                    .expect("a range of rows");
                joined.extend(values(&part));
            }
            assert_eq!(joined, expected, "split at {split}");
        }
        let (_, arena) = placed_string_rows(&LogicalType::Varchar, &pieces, &inverse, 1..3)
            .expect("the middle rows")
            .text_parts()
            .map(|(views, arena)| (views.len(), arena.to_vec()))
            .expect("views");
        assert_eq!(arena, b"the first string past twelve bytes", "only the range's own strings");
        assert!(placed_string_rows(&LogicalType::Varchar, &pieces, &inverse, 4..6).is_err());
    }

    #[test]
    fn an_interleave_of_dictionaries_merges_them_into_one() {
        let word = |text: &str| Value::Varchar(text.to_string());
        let first = [word("MAIL"), word("a word long enough to leave the inline view")];
        let second = [Value::Null, word("MAIL"), word("SHIP")];
        let first = Arc::new(Vector::from_values(LogicalType::Varchar, &first).expect("words"));
        let second = Arc::new(Vector::from_values(LogicalType::Varchar, &second).expect("words"));
        let over = |codes: Vec<u32>, dictionary: &Arc<Vector>| {
            Vector::dictionary_over(codes, Arc::clone(dictionary)).expect("a dictionary")
        };
        let pieces = [
            over((0..16).map(|row| row % 2).collect(), &first),
            over((0..16).map(|row| row % 3).collect(), &second),
            over(vec![1; 8], &first),
        ];
        let order: Vec<usize> = (0..40).rev().collect();
        let laid: Vec<Value> = pieces.iter().flat_map(values).collect();
        let expected: Vec<Value> = order.iter().map(|&index| laid[index].clone()).collect();
        let got = interleave(&LogicalType::Varchar, &pieces, &order).expect("an interleave");
        assert_eq!(values(&got), expected);
        let (_, merged) = got.stable_dictionary_parts().expect("one stable dictionary");
        assert_eq!(merged.len(), 4, "MAIL once, the long word, the null and SHIP");
        let mut inverse = vec![0u32; order.len()];
        for (to, &from) in order.iter().enumerate() {
            inverse[from] = u32::try_from(to).expect("a small row");
        }
        let placed = interleave_placed(&LogicalType::Varchar, &pieces, &order, Some(&inverse))
            .expect("a placed interleave");
        assert_eq!(values(&placed), expected, "placed codes land where pulled ones do");
        assert!(placed.stable_dictionary_parts().is_some(), "and stay one dictionary");

        let mixed = [pieces[0].clone(), pieces[1].flatten().expect("flat")];
        let got = interleave(&LogicalType::Varchar, &mixed, &order[8..]).expect("an interleave");
        assert!(got.dictionary_parts().is_none(), "a flat piece gathers flat");
        let few = &pieces[..1];
        let got = interleave(&LogicalType::Varchar, few, &[3, 2]).expect("an interleave");
        assert_eq!(
            values(&got),
            vec![word("a word long enough to leave the inline view"), word("MAIL")]
        );
    }

    #[test]
    fn a_pick_takes_each_row_from_the_source_it_names_and_is_null_where_none_is_named() {
        let ty = LogicalType::Integer;
        let left =
            Vector::from_values(ty.clone(), &[Value::Integer(1), Value::Null, Value::Integer(3)])
                .expect("left");
        let right = Vector::from_values(ty.clone(), &[Value::Integer(10), Value::Integer(20)])
            .expect("right");
        let picks = [(1, 1), (0, 0), (crate::NO_ROW, 0), (0, 1), (1, 0), (0, 2)];
        let out = picked(&ty, &[&left, &right], &picks).expect("picks").expect("flat sources");
        assert_eq!(
            values(&out),
            vec![
                Value::Integer(20),
                Value::Integer(1),
                Value::Null,
                Value::Null,
                Value::Integer(10),
                Value::Integer(3),
            ]
        );
        let word = |text: &str| Value::Varchar(text.to_string());
        let words =
            Vector::from_values(LogicalType::Varchar, &[word("a"), word("bb")]).expect("words");
        let out = picked(&LogicalType::Varchar, &[&words], &[(0, 1), (0, 0), (0, 1)])
            .expect("picks")
            .expect("flat source");
        assert_eq!(values(&out), vec![word("bb"), word("a"), word("bb")]);
        let coded = Vector::dictionary(vec![1, 1, 0], words.clone()).expect("a dictionary");
        let out = picked(&LogicalType::Varchar, &[&words, &coded], &[(1, 2), (0, 0), (1, 0)])
            .expect("picks")
            .expect("flat and dictionary sources");
        assert_eq!(values(&out), vec![word("a"), word("a"), word("bb")]);
    }

    /// Sources that are codes into one stable dictionary give codes into it, with a pick of no
    /// source and a null row both null, and two dictionaries give strings.
    #[test]
    fn picks_over_one_shared_dictionary_stay_codes() {
        let word = |text: &str| Value::Varchar(text.to_string());
        let words = Arc::new(
            Vector::from_values(LogicalType::Varchar, &[word("a"), word("bb")]).expect("words"),
        );
        let first = Vector::stable_dictionary(vec![1, 0], Arc::clone(&words)).expect("codes");
        let second = Vector::stable_dictionary(vec![0, 1], Arc::clone(&words))
            .expect("codes")
            .with_validity(Validity::from_run(&[true, false]));
        let picks = [(1, 0), (0, 0), (crate::NO_ROW, 0), (1, 1), (0, 1)];
        let out = picked(&LogicalType::Varchar, &[&first, &second], &picks)
            .expect("picks")
            .expect("coded sources");
        let (_, held) = out.stable_dictionary_parts().expect("still codes");
        assert!(Arc::ptr_eq(held, &words), "the codes point somewhere else");
        assert_eq!(values(&out), vec![word("a"), word("bb"), Value::Null, Value::Null, word("a")]);
        let other = Arc::new(Vector::clone(&words));
        let apart = Vector::stable_dictionary(vec![1, 0], other).expect("codes");
        let out = picked(&LogicalType::Varchar, &[&first, &apart], &[(1, 0), (0, 0)])
            .expect("picks")
            .expect("dictionary sources");
        assert!(out.stable_dictionary_parts().is_none(), "two dictionaries are not one");
        assert_eq!(values(&out), vec![word("bb"), word("bb")]);
    }
}
