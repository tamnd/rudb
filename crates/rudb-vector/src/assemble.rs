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

use std::collections::HashMap;
use std::sync::Arc;

use rudb_common::{Error, LogicalType, Result, Value};

use crate::buffer::Buffer;
use crate::string::{Arenas, StringView};
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
pub fn concat(ty: &LogicalType, pieces: &[Vector]) -> Result<Option<Vector>> {
    if pieces.is_empty() {
        return Ok(None);
    }
    let rows = pieces.iter().map(Vector::len).sum();
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
    // Checked before anything is copied, because the fallback is for the caller to keep the pieces
    // it already has and a half built page would be work thrown away.
    let laid = pieces
        .iter()
        .all(|piece| piece.form() == Form::Flat && piece.logical_type() == ty && !piece.is_empty());
    if !laid {
        return Ok(None);
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
        if let Some(placed) = placed_strings(ty, pieces, inverse)? {
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
fn placed_strings(ty: &LogicalType, pieces: &[Vector], inverse: &[u32]) -> Result<Option<Vector>> {
    if !matches!(ty, LogicalType::Varchar | LogicalType::Blob)
        || pieces.iter().any(|piece| piece.text_parts().is_none())
    {
        return Ok(None);
    }
    let rows = inverse.len();
    let mut offsets = vec![0u64; rows + 1];
    let mut places = inverse.iter();
    for piece in pieces {
        let (views, _) = piece.text_parts().unwrap_or_default();
        for (view, &to) in views.iter().zip(places.by_ref()) {
            if view.is_inline() {
                continue;
            }
            if let Some(slot) = offsets.get_mut(to as usize + 1) {
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
            let to = to as usize;
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
fn run_of(pieces: &[Vector], rows: usize) -> Validity {
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
fn arenas_of(pieces: &[Vector]) -> Arenas {
    let mut arenas = Arenas::default();
    for piece in pieces {
        if let Some(Data::Varlen(column)) = piece.data() {
            arenas.count(column);
        }
    }
    arenas
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
        assert!(concat(&ty, &[]).expect("no error").is_none(), "nothing laid into something");
        // A piece of another type is the caller's mistake and is still answered as a layout it will
        // not build, because the fallback keeps the pieces and keeping them is always correct.
        let other =
            Vector::from_values(LogicalType::Integer, &[Value::Integer(1)]).expect("an int");
        assert!(concat(&ty, &[flat, other]).expect("no error").is_none(), "two types laid");
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
}
