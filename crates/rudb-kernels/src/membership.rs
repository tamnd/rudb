//! `IN` over a list the query wrote out.
//!
//! The binder has no `IN` node. `x IN (1, 2, 3)` is bound as `x = 1 OR x = 2 OR x = 3` and
//! `x NOT IN (1, 2, 3)` as `x <> 1 AND x <> 2 AND x <> 3`, which is the right thing for the binder
//! to do because it means nothing after it has to know a second set of rules for null. What it
//! costs is a pass over the column and an output vector per list entry, and TPC-H query 16 has
//! eight entries in one list.
//!
//! This file is the other end of that. A caller that can see the whole conjunction folds it back
//! into a set, and then the column is read once and each row is one lookup. What is being removed
//! is the pass and the allocation per entry rather than the comparison per row, which is why two
//! entries is already worth folding rather than four or eight.
//!
//! # What it will not fold
//!
//! Whole numbers and strings, and nothing else. A float will not fold because DuckDB's `=` on a
//! float is not the equality a hash set has: it says that two nans are equal and that a positive
//! and a negative zero are equal, and the second one also breaks hashing rather than only the
//! comparison. A decimal will not fold because two unscaled integers at two scales are the same
//! number, and the check that the scales match is not worth writing for a list nobody writes. An
//! interval will not fold because interval equality is by length and a month is not thirty days.
//! Anything this refuses stays the `OR` the binder built, which is correct and is counted.

use std::collections::HashSet;
use std::sync::{Arc, OnceLock};

use rudb_common::{LogicalType, Result, Value};
use rudb_vector::{Data, Form, Live, Selection, Validity, Vector};

use crate::fallback::{self, Kernel};
use crate::peel::{Found, Peel, search};
use crate::shape::{first, identity, nulls_of, single};

/// The list of an `IN`, in the shape a loop can look a row up in.
///
/// Built once when the pipeline is built, because the list is literals the user wrote and cannot
/// change from chunk to chunk. This is the same idea as [`crate::prepare`] and is a separate type
/// only because an `IN` is not a function call by the time it reaches here.
#[derive(Debug)]
pub struct Members {
    held: Held,
    /// Whether the list held a null.
    ///
    /// A row that is not in the list is null rather than false when it did, because the row might
    /// have equalled whatever the null stands for. This is the whole of the difference between an
    /// `IN` and a set lookup and it is the thing a hand written version gets wrong.
    has_null: bool,
    /// Whether this was a `NOT IN`, which the binder wrote as an `AND` of inequalities.
    negated: bool,
    /// Where the list's values sit in the dictionary a column arrives with, when that dictionary
    /// came with its own sorted order. Searched once for the whole query.
    sought: OnceLock<Sought>,
    /// The per value memo for a dictionary that has no order to search.
    peel: Peel,
}

/// The codes one list sits at in one dictionary, searched once and remembered.
///
/// The same idea as [`crate::peel::Lookup`], which does it for a single literal, and a separate
/// type because a list is several literals and the answer is therefore a set of codes rather than
/// one. Short, since it is as long as the list the query wrote out.
#[derive(Debug)]
struct Sought {
    /// The dictionary these codes are in, recognised by pointer the way a peel does it.
    dictionary: Arc<Vector>,
    /// Whether each code of the dictionary is one of the list's values, so a row costs one load
    /// rather than a walk along the codes the list sits at. A value the dictionary does not hold
    /// marks nothing, since no row can be that value.
    hits: Vec<bool>,
}

/// The set itself, in the one layout per kind of value that hashes the way SQL compares.
#[derive(Debug)]
enum Held {
    /// Every integral type and the three whole calendar ones, widened to the widest signed integer.
    /// Widening is exact for all of them, and the binder has already cast the column and the list
    /// to one type, so two entries that differ here differ in SQL too.
    Whole(Whole),
    /// Strings, compared by bytes, which is what DuckDB's `=` on a varchar does.
    Text(Text),
}

/// A list of whole numbers, kept as the list itself while it is short.
///
/// Most lists a query writes out are two or three values, like `TraficSourceID IN (-1, 6)`, and
/// hashing a row with the standard library's keyed hash to look one of those up cost more than the
/// rest of the filter it sat in. Comparing against every entry of a short list is a few compares a
/// row and nothing else, so the hash set is only built past [`SHORT`] entries.
#[derive(Debug)]
struct Whole {
    short: Vec<i128>,
    set: HashSet<i128>,
}

/// The longest list compared entry by entry rather than hashed.
const SHORT: usize = 8;

impl Whole {
    fn of(set: HashSet<i128>) -> Self {
        if set.len() <= SHORT {
            Self { short: set.into_iter().collect(), set: HashSet::new() }
        } else {
            Self { short: Vec::new(), set }
        }
    }

    #[inline]
    fn contains(&self, value: &i128) -> bool {
        if self.set.is_empty() { self.short.contains(value) } else { self.set.contains(value) }
    }

    fn len(&self) -> usize {
        self.short.len() + self.set.len()
    }
}

/// A list of strings, kept as the list itself while it is short, and compared by bytes.
///
/// The same reasoning as [`Whole`]. TPC-H q22 asks whether the first two characters of a phone
/// number are one of seven codes, and hashing each two byte answer with the keyed hash after
/// checking it was text was a sixth of the query. A value read out of a column is already known to
/// be text, so the check found nothing, and seven two byte compares are cheaper than one hash.
///
/// A short list whose every entry fits in eight bytes is kept as words, each entry's bytes and its
/// length, so a row is a load and a compare per entry with no call to compare bytes. The two bytes
/// of a q22 country code went through `memcmp` seven times a row otherwise, and that was a third of
/// what was left of the query after the hash went.
#[derive(Debug)]
struct Text {
    words: Vec<(u64, usize)>,
    short: Vec<Box<[u8]>>,
    set: HashSet<Box<[u8]>>,
}

/// The bytes of a value of at most eight bytes as one word, the first byte lowest.
#[inline]
fn word(value: &[u8]) -> u64 {
    let mut bytes = [0_u8; 8];
    bytes[..value.len()].copy_from_slice(value);
    u64::from_le_bytes(bytes)
}

impl Text {
    fn of(set: HashSet<String>) -> Self {
        let held = set.into_iter().map(|text| text.into_bytes().into_boxed_slice());
        if held.len() > SHORT {
            return Self { words: Vec::new(), short: Vec::new(), set: held.collect() };
        }
        let short: Vec<Box<[u8]>> = held.collect();
        if short.iter().all(|text| text.len() <= 8) {
            let words = short.iter().map(|text| (word(text), text.len())).collect();
            return Self { words, short, set: HashSet::new() };
        }
        Self { words: Vec::new(), short, set: HashSet::new() }
    }

    #[inline]
    fn contains(&self, value: &[u8]) -> bool {
        if !self.words.is_empty() {
            if value.len() > 8 {
                return false;
            }
            let value = (word(value), value.len());
            return self.words.contains(&value);
        }
        if self.set.is_empty() {
            self.short.iter().any(|held| **held == *value)
        } else {
            self.set.contains(value)
        }
    }

    fn iter(&self) -> impl Iterator<Item = &[u8]> {
        self.short.iter().chain(self.set.iter()).map(|held| &**held)
    }

    fn len(&self) -> usize {
        self.short.len() + self.set.len()
    }
}

impl Members {
    /// The list as a set, or `None` for a list this file will not fold.
    ///
    /// `None` covers a list of fewer than two entries, which is not worth a set, a list holding a
    /// kind of value that does not hash the way SQL compares, and a list mixing two kinds, which
    /// the binder does not produce but which is cheaper to refuse than to reason about.
    #[must_use]
    pub fn of(values: &[Value], negated: bool) -> Option<Self> {
        if values.len() < 2 {
            return None;
        }
        let mut whole: HashSet<i128> = HashSet::new();
        let mut text: HashSet<String> = HashSet::new();
        let mut has_null = false;
        let mut kind: Option<std::mem::Discriminant<Value>> = None;
        for value in values {
            if matches!(value, Value::Null) {
                has_null = true;
                continue;
            }
            // One kind for the whole list. The binder casts every entry to the type the comparison
            // happens at, so a list that reaches here is already uniform, and a list that is not is
            // one this file has no business guessing about.
            let held = std::mem::discriminant(value);
            if *kind.get_or_insert(held) != held {
                return None;
            }
            match value {
                Value::Varchar(held) => {
                    text.insert(held.clone());
                }
                other => {
                    whole.insert(number(other)?);
                }
            }
        }
        let held = if text.is_empty() {
            if whole.is_empty() {
                // Every entry was null, so every row is null and there is nothing to look up. Rare
                // enough that the `OR` can have it.
                return None;
            }
            Held::Whole(Whole::of(whole))
        } else {
            Held::Text(Text::of(text))
        };
        Some(Self { held, has_null, negated, sought: OnceLock::new(), peel: Peel::default() })
    }

    /// Which codes of `column`'s dictionary hold one of this list's values, or `None` when there is no
    /// sorted order to find them with.
    ///
    /// This is the whole of what a sorted dictionary buys an `IN`. The list is a handful of
    /// literals and the dictionary knows where each of them sits, so one binary search per literal
    /// for the whole query turns the predicate into a code against a handful of codes, and no
    /// value is read at any point. `l_shipmode IN ('MAIL', 'SHIP')` over SF1 lineitem is two
    /// searches of a seven entry dictionary rather than six million string comparisons.
    ///
    /// Text only, because the search is over bytes. A list of numbers against a dictionary is left
    /// to the loop below, which reads the codes' values as a run and is already one lookup a row.
    fn sought(&self, column: &Vector) -> Option<Result<&[bool]>> {
        let Held::Text(set) = &self.held else { return None };
        let (_, dictionary) = column.shared_dictionary_parts()?;
        if self.sought.get().is_none() {
            let ranks = dictionary.ranks()?;
            let mut hits = vec![false; dictionary.len()];
            for text in set.iter() {
                match search(dictionary, ranks, text) {
                    Ok(Found::At(code)) => {
                        if let Some(hit) = hits.get_mut(code as usize) {
                            *hit = true;
                        }
                    }
                    Ok(Found::Absent) => {}
                    // Returned rather than remembered, so a caller that retries gets the error
                    // again rather than a wrong answer cached from a half finished search.
                    Err(error) => return Some(Err(error)),
                }
            }
            // Two threads that get here at once do the same searches and set the same codes, and
            // the one that loses the race drops its own copy of them.
            let _ = self.sought.set(Sought { dictionary: Arc::clone(dictionary), hits });
        }
        // Read back what is actually there rather than what this call built, and check it belongs
        // to the dictionary in hand, which is what declines a second column at the same node.
        let memo = self.sought.get()?;
        Arc::ptr_eq(&memo.dictionary, dictionary).then_some(Ok(memo.hits.as_slice()))
    }

    /// Whether the value at `code` of `dictionary` is in the list, for the memo to remember.
    fn at_code(&self, dictionary: &Vector, code: usize) -> Result<bool> {
        Ok(match &self.held {
            Held::Text(set) => {
                dictionary.try_bytes_at(code)?.is_some_and(|text| set.contains(text))
            }
            Held::Whole(set) => {
                number(&dictionary.try_value_at(code)?).is_some_and(|held| set.contains(&held))
            }
        })
    }

    /// How many distinct values the list holds, for a caller that wants to say so.
    #[must_use]
    pub fn len(&self) -> usize {
        match &self.held {
            Held::Whole(set) => set.len(),
            Held::Text(set) => set.len(),
        }
    }

    /// Whether the list holds no value at all, which [`Members::of`] never builds.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

/// A value as the integer the set is keyed on, or `None` for a kind that does not belong in one.
fn number(value: &Value) -> Option<i128> {
    match *value {
        Value::TinyInt(held) => Some(i128::from(held)),
        Value::SmallInt(held) => Some(i128::from(held)),
        Value::Integer(held) | Value::Date(held) => Some(i128::from(held)),
        Value::BigInt(held)
        | Value::Time(held)
        | Value::TimeTz(held)
        | Value::Timestamp(held)
        | Value::TimestampTz(held) => Some(i128::from(held)),
        Value::HugeInt(held) => Some(held),
        Value::UTinyInt(held) => Some(i128::from(held)),
        Value::USmallInt(held) => Some(i128::from(held)),
        Value::UInteger(held) => Some(i128::from(held)),
        Value::UBigInt(held) => Some(i128::from(held)),
        _ => None,
    }
}

/// Which rows of `input` are in the list.
///
/// # Errors
///
/// If the answer vector cannot be built, which is the same check every kernel here makes.
pub fn in_set(input: &Vector, members: &Members, returns: &LogicalType) -> Result<Vector> {
    let rows = input.len();
    let base = nulls_of(input);
    match input.form() {
        Form::Flat => match input.data() {
            Some(data) => look(data, identity, members, &base, rows, returns),
            None => row_at_a_time(input, members, &base, rows, returns),
        },
        Form::Dictionary | Form::Rle => {
            // A dictionary column asks the same question about the same value once a row, so both
            // paths here ask it once a value instead. The search goes first because it reads no
            // value at all, and the memo takes the dictionaries that have no order to search.
            if let Some(found) = members.sought(input) {
                let found = found?;
                let (codes, _) = input.shared_dictionary_parts().ok_or_else(|| {
                    rudb_common::Error::internal("a searched column lost its codes")
                })?;
                if codes.len() >= rows {
                    return answer(rows, &base, members, returns, |index| {
                        found.get(codes[index] as usize).copied().unwrap_or(false)
                    });
                }
            }
            if let Some(found) = members
                .peel
                .answer(input, rows, identity, |dictionary, code| members.at_code(dictionary, code))
            {
                let found = found?;
                return answer(rows, &base, members, returns, |index| found[index]);
            }
            let Some((codes, values)) = input.positions() else {
                return row_at_a_time(input, members, &base, rows, returns);
            };
            if codes.len() < rows {
                return row_at_a_time(input, members, &base, rows, returns);
            }
            let at = move |index: usize| codes[index] as usize;
            match (values.data(), values.packed_parts()) {
                (Some(data), _) => look(data, at, members, &base, rows, returns),
                // A dictionary whose distinct values are themselves packed, which is the pair of
                // forms a narrow column written to a file arrives in.
                (None, Some(packed)) => packed_look(&packed, at, members, &base, rows, returns),
                (None, None) => row_at_a_time(input, members, &base, rows, returns),
            }
        }
        // Whole numbers in as many bits as the column's range needs, which is the form every narrow
        // integer column of ClickBench is in. Without this arm the walk below reads a value a row
        // out of a packed run, and reading one of those allocates twice, so a two entry `IN` over
        // `TraficSourceID` cost more than the four comparisons of the rest of query 40 put together.
        Form::BitPacked => match input.packed_parts() {
            Some(packed) => packed_look(&packed, identity, members, &base, rows, returns),
            None => row_at_a_time(input, members, &base, rows, returns),
        },
        Form::Constant => {
            let Some(value) = input.constant_value() else {
                return row_at_a_time(input, members, &base, rows, returns);
            };
            let Some(held) = single(input.logical_type(), value) else {
                return row_at_a_time(input, members, &base, rows, returns);
            };
            match held.data() {
                Some(data) => look(data, first, members, &base, rows, returns),
                None => row_at_a_time(input, members, &base, rows, returns),
            }
        }
        _ => row_at_a_time(input, members, &base, rows, returns),
    }
}

/// The rows of `input` in the list, out of `live` when there is one and out of every row when not.
///
/// The filter's half of [`in_set`]. That builds a flag a row over the whole chunk and the filter then
/// reads the flags back out at the rows still in play, so an `IN` after a selective conjunct did all
/// of its work on rows already thrown away. ClickBench 40 is that: `RefererHash` keeps one row in
/// eight and `TraficSourceID IN (-1, 6)` was looked up on all eight. Here the rows come out as a
/// selection directly, each one compared against the list in the column's own width, so a row is a
/// load and a couple of compares rather than a widening to 128 bits and a walk along a vector.
///
/// A list of strings over a dictionary column is answered once a code, the way [`in_set`] answers
/// it, but only for the codes of the rows in play. q12's `l_shipmode IN ('MAIL', 'SHIP')` runs after
/// the date conjuncts have kept about one row in seven, and it built its flags on all seven.
///
/// `None` for anything but a short list of whole numbers with no null in it, over a flat or packed
/// column with no null in it, or a list of strings with no null in it over a dictionary column with
/// no null in it. The caller falls back to [`in_set`] for the rest, which also reports any error a
/// lookup here met rather than this deciding what it means.
#[must_use]
pub fn select_in(input: &Vector, members: &Members, live: Option<&Selection>) -> Option<Selection> {
    let set = match &members.held {
        Held::Whole(set) => set,
        Held::Text(_) => return select_text(input, members, live),
    };
    if members.has_null || !set.set.is_empty() || !input.none_null() {
        return None;
    }
    let rows = input.len();
    u32::try_from(rows).ok()?;
    let negated = members.negated;
    let named = live.map(Selection::indices);
    if named.is_some_and(|named| named.iter().any(|&row| row as usize >= rows)) {
        return None;
    }
    let count = named.map_or(rows, <[u32]>::len);
    /// The list in the column's width, leaving out what that width cannot hold, since no row can.
    macro_rules! flat {
        ($values:expr, $ty:ty) => {{
            let values = $values.as_slice();
            let wanted: Vec<$ty> =
                set.short.iter().filter_map(|&value| <$ty>::try_from(value).ok()).collect();
            Some(match named {
                Some(named) => chosen(|slot| values[named[slot] as usize], &wanted, negated, named),
                None => chosen_all(values, &wanted, negated),
            })
        }};
    }
    match input.form() {
        Form::Flat => match input.data()? {
            Data::Int8(values) => flat!(values, i8),
            Data::Int16(values) => flat!(values, i16),
            Data::Int32(values) => flat!(values, i32),
            Data::Int64(values) => flat!(values, i64),
            Data::UInt8(values) => flat!(values, u8),
            Data::UInt16(values) => flat!(values, u16),
            Data::UInt32(values) => flat!(values, u32),
            Data::UInt64(values) => flat!(values, u64),
            _ => None,
        },
        Form::BitPacked => {
            let packed = input.packed_parts()?;
            let wanted: Vec<u64> =
                set.short.iter().filter_map(|&value| packed.code_of(value)).collect();
            Some(match named {
                Some(named) => {
                    let codes = packed.codes_at(|slot| named[slot] as usize, count);
                    chosen(|slot| codes[slot], &wanted, negated, named)
                }
                None => {
                    let mut codes = vec![0; rows];
                    packed.unpack(0, &mut codes);
                    chosen_all(&codes, &wanted, negated)
                }
            })
        }
        _ => None,
    }
}

/// [`select_in`] for a list of strings, over the codes of a dictionary column.
fn select_text(input: &Vector, members: &Members, live: Option<&Selection>) -> Option<Selection> {
    if members.has_null || !input.none_null() || !matches!(input.form(), Form::Dictionary) {
        return None;
    }
    let rows = input.len();
    u32::try_from(rows).ok()?;
    let named = live.map(Selection::indices);
    let (codes, _) = input.shared_dictionary_parts()?;
    if codes.len() < rows
        || named.is_some_and(|named| named.iter().any(|&row| row as usize >= rows))
    {
        return None;
    }
    let count = named.map_or(rows, <[u32]>::len);
    #[expect(clippy::cast_possible_truncation, reason = "the row count was checked to fit a u32")]
    let row = |slot: usize| named.map_or(slot as u32, |named| named[slot]);
    let negated = members.negated;
    // The sorted search reads no value at all, and the memo takes the dictionaries without an order.
    if let Some(found) = members.sought(input) {
        let found = found.ok()?;
        return Some(picked(count, row, |slot| {
            found.get(codes[row(slot) as usize] as usize).copied().unwrap_or(false) != negated
        }));
    }
    let flags = members
        .peel
        .answer(
            input,
            count,
            |slot| row(slot) as usize,
            |dictionary, code| members.at_code(dictionary, code),
        )?
        .ok()?;
    Some(picked(count, row, |slot| flags[slot] != negated))
}

/// The rows `row` names for each slot below `count` that `keep` holds for, written without a
/// branch on the answer.
fn picked(count: usize, row: impl Fn(usize) -> u32, keep: impl Fn(usize) -> bool) -> Selection {
    let mut out = vec![0_u32; count];
    let mut kept = 0;
    for slot in 0..count {
        out[kept] = row(slot);
        kept += usize::from(keep(slot));
    }
    out.truncate(kept);
    Selection::from_indices(out)
}

/// Every row of `values` that is one of `wanted`, or that is none of them when `negated`.
#[expect(
    clippy::cast_possible_truncation,
    reason = "the caller checked that the row count fits in a u32"
)]
fn chosen_all<T: Copy + PartialEq>(values: &[T], wanted: &[T], negated: bool) -> Selection {
    let mut out = vec![0_u32; values.len()];
    let mut kept = 0;
    for (row, &value) in values.iter().enumerate() {
        out[kept] = row as u32;
        kept += usize::from(among(value, wanted) != negated);
    }
    out.truncate(kept);
    Selection::from_indices(out)
}

/// [`chosen_all`] over the rows `named` names, with slot `i` of `value` being row `named[i]`.
fn chosen<T: Copy + PartialEq>(
    value: impl Fn(usize) -> T,
    wanted: &[T],
    negated: bool,
    named: &[u32],
) -> Selection {
    let mut out = vec![0_u32; named.len()];
    let mut kept = 0;
    for (slot, &row) in named.iter().enumerate() {
        out[kept] = row;
        kept += usize::from(among(value(slot), wanted) != negated);
    }
    out.truncate(kept);
    Selection::from_indices(out)
}

/// Whether `value` is one of `wanted`, with no branch for the lists a query writes out by hand.
#[inline]
fn among<T: Copy + PartialEq>(value: T, wanted: &[T]) -> bool {
    match *wanted {
        [] => false,
        [one] => value == one,
        [one, two] => (value == one) | (value == two),
        [one, two, three] => (value == one) | (value == two) | (value == three),
        _ => wanted.iter().fold(false, |found, &each| found | (value == each)),
    }
}

/// The lookup loop, once per physical layout the column can arrive in.
///
/// The index mapping is a generic parameter rather than a function pointer for the reason the
/// `by_form` macro in `scalar` gives, which is that a function pointer here is an indirect call per
/// row.
fn look<A: Fn(usize) -> usize>(
    data: &Data,
    at: A,
    members: &Members,
    base: &Validity,
    rows: usize,
    returns: &LogicalType,
) -> Result<Vector> {
    match (&members.held, data) {
        (Held::Text(set), Data::Varlen(column)) => answer(rows, base, members, returns, |index| {
            column.bytes(at(index)).is_some_and(|text| set.contains(text))
        }),
        (Held::Whole(set), Data::Int8(held)) => {
            answer(rows, base, members, returns, |index| holds(set, held.as_slice(), at(index)))
        }
        (Held::Whole(set), Data::Int16(held)) => {
            answer(rows, base, members, returns, |index| holds(set, held.as_slice(), at(index)))
        }
        (Held::Whole(set), Data::Int32(held)) => {
            answer(rows, base, members, returns, |index| holds(set, held.as_slice(), at(index)))
        }
        (Held::Whole(set), Data::Int64(held)) => {
            answer(rows, base, members, returns, |index| holds(set, held.as_slice(), at(index)))
        }
        (Held::Whole(set), Data::Int128(held)) => {
            answer(rows, base, members, returns, |index| holds(set, held.as_slice(), at(index)))
        }
        (Held::Whole(set), Data::UInt8(held)) => {
            answer(rows, base, members, returns, |index| holds(set, held.as_slice(), at(index)))
        }
        (Held::Whole(set), Data::UInt16(held)) => {
            answer(rows, base, members, returns, |index| holds(set, held.as_slice(), at(index)))
        }
        (Held::Whole(set), Data::UInt32(held)) => {
            answer(rows, base, members, returns, |index| holds(set, held.as_slice(), at(index)))
        }
        (Held::Whole(set), Data::UInt64(held)) => {
            answer(rows, base, members, returns, |index| holds(set, held.as_slice(), at(index)))
        }
        // A layout the set cannot be keyed on, which means the list and the column disagree about
        // what they hold. `Members::of` refuses the lists that would get here, so this is the arm
        // that keeps that true rather than assumed.
        _ => Err(rudb_common::Error::internal(format!(
            "an IN list over a column this kernel does not read, which is {returns}"
        ))),
    }
}

/// The same lookup over a packed run, whose value is its base plus its code.
///
/// Separate from [`look`] rather than a layout inside it because a packed run has no flat layout to
/// match on: the value is computed from the bits rather than read out of an array. The index mapping
/// is generic for the same reason it is there, so a dictionary that points into a packed run of
/// distinct values comes through here with its codes instead of falling to the row at a time walk.
fn packed_look<A: Fn(usize) -> usize>(
    packed: &rudb_vector::Packed<'_>,
    at: A,
    members: &Members,
    base: &Validity,
    rows: usize,
    returns: &LogicalType,
) -> Result<Vector> {
    match &members.held {
        Held::Whole(set) => answer(rows, base, members, returns, |index| {
            set.contains(&(packed.base() + i128::from(packed.code(at(index)))))
        }),
        // A run of packed integers against a list of strings, which the binder does not produce
        // because it casts both sides to one type first. No row matches, which is what the row at a
        // time walk answers for the same pairing, so the two paths agree rather than one erroring.
        Held::Text(_) => answer(rows, base, members, returns, |_| false),
    }
}

/// Whether the set holds the value at `index`, for any integer narrower than the key.
fn holds<T: Copy>(set: &Whole, values: &[T], index: usize) -> bool
where
    i128: From<T>,
{
    values.get(index).is_some_and(|&held| set.contains(&i128::from(held)))
}

/// The answer, given a lookup that says whether a row is in the list.
fn answer(
    rows: usize,
    base: &Validity,
    members: &Members,
    returns: &LogicalType,
    found: impl Fn(usize) -> bool,
) -> Result<Vector> {
    // Nothing null in the column and nothing null in the list, which is nearly every `IN` there is.
    // Every row then has an answer, so there is no second run of flags to fill and pack.
    if base.live() == Live::All && !members.has_null {
        let out: Vec<bool> = (0..rows).map(|index| found(index) != members.negated).collect();
        return Vector::flat(returns.clone(), Data::Bool(out.into()));
    }
    let mut out = vec![false; rows];
    let mut live = vec![false; rows];
    for index in 0..rows {
        if !base.is_valid(index) {
            continue;
        }
        let hit = found(index);
        // A miss against a list with a null in it is null and not false, because the row might have
        // equalled whatever that null stands for. A hit is a hit whatever else the list holds.
        live[index] = hit || !members.has_null;
        out[index] = hit != members.negated;
    }
    let validity = Validity::from_run(&live).normalize(rows);
    Ok(Vector::flat(returns.clone(), Data::Bool(out.into()))?.with_validity(validity))
}

/// The path for a form or a layout with no loop above, which reads a value per row.
///
/// It used to count itself nowhere, on the reasoning that a column reaching here is one
/// `Members::of` should not have folded, so the fix would be a line there rather than a number in a
/// report. That was wrong, and it is worth writing down why rather than quietly deleting it. Every
/// narrow integer column of ClickBench is bit packed and this kernel had no arm for that form, so
/// the fold was right and the dispatch below it was not. Nothing said so: the fallback table is the
/// one place anybody looks for a kernel reading a value at a time, and this kernel was not in it, so
/// query 40 spent most of its scan here for as long as the file has existed.
///
/// Both forms are the input's, since the list is a set rather than a vector and there is no second
/// form to report. The same shape as the select kernel, which reports on one vector too.
fn row_at_a_time(
    input: &Vector,
    members: &Members,
    base: &Validity,
    rows: usize,
    returns: &LogicalType,
) -> Result<Vector> {
    fallback::record(Kernel::Membership, input.form(), input.form());
    let held: Vec<Value> =
        (0..rows).map(|index| input.try_value_at(index)).collect::<Result<_>>()?;
    answer(rows, base, members, returns, |index| match (&members.held, &held[index]) {
        (Held::Text(set), Value::Varchar(text)) => set.contains(text.as_bytes()),
        (Held::Whole(set), value) => number(value).is_some_and(|held| set.contains(&held)),
        _ => false,
    })
}

#[cfg(test)]
mod tests {
    use std::cmp::Ordering;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering as Memory};

    use rudb_common::{LogicalType, Value};
    use rudb_vector::{Form, Selection, Vector};

    use super::{Kernel, Members, fallback, in_set, select_in};

    /// What the kernel answers for each row, as the values a caller would read back.
    fn over(input: &Vector, list: &[Value], negated: bool) -> Vec<Value> {
        let members = Members::of(list, negated).expect("this list folds");
        let answer = in_set(input, &members, &LogicalType::Boolean).expect("the lookup runs");
        (0..input.len()).map(|row| answer.value_at(row)).collect()
    }

    fn numbers() -> Vector {
        Vector::from_values(
            LogicalType::Integer,
            &[Value::Integer(1), Value::Integer(7), Value::Null, Value::Integer(3)],
        )
        .expect("four integers")
    }

    #[test]
    fn a_text_list_matches_whole_values_whatever_their_length() {
        let texts = ["13", "1", "", "130", "é", "éé", "exactly8", "exactly8!", "31"];
        let input = Vector::from_values(
            LogicalType::Varchar,
            &texts.iter().map(|text| Value::Varchar((*text).into())).collect::<Vec<_>>(),
        )
        .expect("strings");
        let text = |value: &str| Value::Varchar(value.into());
        let lists: [&[&str]; 4] = [
            &["13", "31", "é"],
            &["", "exactly8", "éé"],
            &["exactly8!", "1"],
            &["a", "b", "c", "d", "e", "f", "g", "h", "13", "éé"],
        ];
        for list in lists {
            let values: Vec<Value> = list.iter().map(|value| text(value)).collect();
            let wanted: Vec<Value> =
                texts.iter().map(|value| Value::Boolean(list.contains(value))).collect();
            assert_eq!(over(&input, &values, false), wanted, "{list:?}");
        }
    }

    #[test]
    fn a_row_in_the_list_is_true_and_a_row_outside_it_is_false() {
        assert_eq!(
            over(&numbers(), &[Value::Integer(1), Value::Integer(3)], false),
            [Value::Boolean(true), Value::Boolean(false), Value::Null, Value::Boolean(true)]
        );
    }

    #[test]
    fn a_not_in_is_the_same_lookup_read_the_other_way() {
        assert_eq!(
            over(&numbers(), &[Value::Integer(1), Value::Integer(3)], true),
            [Value::Boolean(false), Value::Boolean(true), Value::Null, Value::Boolean(false)]
        );
    }

    /// A list past the length compared entry by entry is hashed, and answers the same.
    #[test]
    fn a_long_list_answers_what_a_short_one_does() {
        let mut long: Vec<Value> = (100..120).map(Value::Integer).collect();
        long.push(Value::Integer(3));
        long.push(Value::Integer(1));
        assert_eq!(
            over(&numbers(), &long, false),
            [Value::Boolean(true), Value::Boolean(false), Value::Null, Value::Boolean(true)]
        );
    }

    /// The rule that separates a set lookup from an `IN`. `7 IN (1, NULL)` is null rather than
    /// false, because the row might have equalled whatever the null stands for, and `7 NOT IN
    /// (1, NULL)` is null for the same reason.
    #[test]
    fn a_miss_against_a_list_with_a_null_in_it_is_null() {
        let list = [Value::Integer(1), Value::Null, Value::Integer(3)];
        assert_eq!(
            over(&numbers(), &list, false),
            [Value::Boolean(true), Value::Null, Value::Null, Value::Boolean(true)]
        );
        assert_eq!(
            over(&numbers(), &list, true),
            [Value::Boolean(false), Value::Null, Value::Null, Value::Boolean(false)]
        );
    }

    #[test]
    fn a_dictionary_column_is_read_through_its_codes() {
        let values = Vector::from_values(
            LogicalType::Varchar,
            &[Value::Varchar("a".into()), Value::Varchar("b".into()), Value::Null],
        )
        .expect("builds");
        let text = Vector::dictionary(vec![0, 2, 1, 0], values).expect("codes are in range");
        let list = [Value::Varchar("a".into()), Value::Varchar("c".into())];
        assert_eq!(
            over(&text, &list, false),
            [Value::Boolean(true), Value::Null, Value::Boolean(false), Value::Boolean(true)]
        );
    }

    /// A packed run, on its own and under a dictionary, which is how a narrow integer column reads.
    ///
    /// The form ClickBench `TraficSourceID` is in, and the one this kernel used to have no arm for.
    /// Falling through to the row at a time walk read a value a row out of the bits, and reading one
    /// of those allocates twice, so query 40's two entry `IN` cost more than the four comparisons
    /// beside it put together. The answers are checked against the same list over the same numbers
    /// written flat, since what the fast arm must not do is answer differently from the slow one.
    #[test]
    fn a_packed_column_is_read_out_of_its_bits_rather_than_a_value_at_a_time() {
        let values: Vec<Value> = (0..40)
            .map(|row| if row % 9 == 0 { Value::Null } else { Value::Integer(row % 12 - 1) })
            .collect();
        let flat = Vector::from_values(LogicalType::Integer, &values).expect("forty integers");
        let packed = flat.clone().bit_packed().expect("a range of twelve packs");
        assert_eq!(packed.form(), Form::BitPacked);
        let list = [Value::Integer(-1), Value::Integer(6)];
        assert_eq!(over(&packed, &list, false), over(&flat, &list, false));
        assert_eq!(over(&packed, &list, true), over(&flat, &list, true));

        // And the pair of forms together, which is what a dictionary over a narrow column is. The
        // codes point at the packed run, so the mapping has to reach the bits rather than stopping
        // at a flat layout that is not there.
        let codes: Vec<u32> = (0..24).map(|row| (row * 7) % 40).collect();
        let over_packed = Vector::dictionary(codes.clone(), packed).expect("codes are in range");
        let over_flat = Vector::dictionary(codes, flat).expect("codes are in range");
        assert_eq!(over_packed.form(), Form::Dictionary);
        assert_eq!(over(&over_packed, &list, false), over(&over_flat, &list, false));

        // And that neither of them reached the row at a time walk, which is the part that matters.
        // Answering the same is necessary and not sufficient: the walk answers the same too, and it
        // is the thing being got rid of. The counter is thread local under test, so these are this
        // test's own calls and nobody else's.
        assert_eq!(fallback::count(Kernel::Membership, Form::BitPacked, Form::BitPacked), 0);
        assert_eq!(fallback::count(Kernel::Membership, Form::Dictionary, Form::Dictionary), 0);
    }

    /// The rows a filter keeps are the rows the flag kernel says yes to, over every row and over a
    /// selection, flat and packed, for `IN` and `NOT IN`. A list entry the column's width cannot
    /// hold is one no row matches, which the flat `TINYINT` case is there for.
    #[test]
    fn selecting_the_rows_in_a_list_keeps_what_the_flags_say() {
        let values: Vec<Value> = (0..300).map(|row| Value::Integer(row % 12 - 1)).collect();
        let flat = Vector::from_values(LogicalType::Integer, &values).expect("integers");
        let packed = flat.clone().bit_packed().expect("a range of twelve packs");
        assert_eq!(packed.form(), Form::BitPacked);
        let tiny: Vec<Value> = (0..300)
            .map(|row| Value::TinyInt(i8::try_from(row % 12 - 1).expect("small")))
            .collect();
        let tiny = Vector::from_values(LogicalType::TinyInt, &tiny).expect("tiny integers");
        let live = Selection::from_indices((0..300).filter(|row| row % 5 != 2).collect());
        let lists = [
            vec![Value::Integer(-1), Value::Integer(6)],
            vec![Value::Integer(3), Value::Integer(9), Value::Integer(4000)],
            vec![Value::Integer(0), Value::Integer(1), Value::Integer(2), Value::Integer(10)],
        ];
        for list in &lists {
            for negated in [false, true] {
                let members = Members::of(list, negated).expect("this list folds");
                let yes = over(&flat, list, negated);
                let kept = |row: usize| yes[row] == Value::Boolean(true);
                let every = Selection::from_predicate(300, kept);
                let among = Selection::from_indices(
                    live.indices().iter().copied().filter(|&row| kept(row as usize)).collect(),
                );
                for column in [&flat, &packed, &tiny] {
                    let all = select_in(column, &members, None).expect("a short whole list");
                    assert_eq!(all, every, "{list:?} negated {negated} over {:?}", column.form());
                    let some = select_in(column, &members, Some(&live)).expect("the same");
                    assert_eq!(some, among, "{list:?} negated {negated} over the live rows");
                }
            }
        }
        // A null in the column or in the list is left to the flag kernel, which knows the rule.
        let nulled = Members::of(&[Value::Integer(1), Value::Null], false).expect("folds");
        assert!(select_in(&flat, &nulled, None).is_none());
        let members = Members::of(&lists[0], false).expect("folds");
        assert!(select_in(&numbers(), &members, None).is_none());
    }

    #[test]
    fn a_constant_column_answers_every_row_the_same() {
        let held = Vector::constant(LogicalType::Integer, Value::Integer(3), 3);
        let list = [Value::Integer(1), Value::Integer(3)];
        assert_eq!(over(&held, &list, false), vec![Value::Boolean(true); 3]);
    }

    #[test]
    fn a_run_length_column_reads_the_same_as_the_flat_one_it_stands_for() {
        let flat = Vector::from_values(
            LogicalType::Integer,
            &[Value::Integer(1), Value::Integer(1), Value::Integer(7), Value::Integer(7)],
        )
        .expect("four integers");
        let runs = flat.clone().run_encoded().expect("two runs");
        let list = [Value::Integer(1), Value::Integer(3)];
        assert_eq!(over(&runs, &list, false), over(&flat, &list, false));
    }

    #[test]
    fn a_list_of_one_is_left_alone_because_a_comparison_is_already_that() {
        assert!(Members::of(&[Value::Integer(1)], false).is_none());
    }

    #[test]
    fn a_list_of_floats_does_not_fold() {
        // Two nans are equal to DuckDB's `=` and not to a hash set, and a positive and a negative
        // zero are equal to both but hash differently. Neither is worth a special case.
        assert!(Members::of(&[Value::Double(1.0), Value::Double(2.0)], false).is_none());
    }

    #[test]
    fn a_list_of_two_kinds_does_not_fold() {
        let mixed = [Value::Integer(1), Value::Varchar("a".into())];
        assert!(Members::of(&mixed, false).is_none());
    }

    #[test]
    fn a_list_of_nothing_but_nulls_does_not_fold() {
        assert!(Members::of(&[Value::Null, Value::Null], false).is_none());
    }

    /// Values a storage reader hands over one at a time, which know the order the writer sorted
    /// them into. This is the shape a text column of a native file arrives in, and the whole point
    /// of the shape is that nothing has to read a value to find out where one sits.
    #[derive(Debug)]
    struct Filed {
        values: Vec<Vec<u8>>,
        order: Vec<u32>,
        /// How many values were read to answer, which is what the searching path drives to zero.
        reads: AtomicUsize,
    }

    impl rudb_vector::TextSource for Filed {
        fn len(&self) -> usize {
            self.values.len()
        }

        fn bytes_at(&self, index: usize) -> rudb_common::Result<Option<&[u8]>> {
            self.reads.fetch_add(1, Memory::Relaxed);
            Ok(self.values.get(index).map(Vec::as_slice))
        }

        fn footprint(&self) -> usize {
            self.values.iter().map(Vec::len).sum()
        }

        fn ranks(&self) -> Option<usize> {
            Some(self.order.len())
        }

        fn compare_rank(&self, rank: usize, wanted: &[u8]) -> rudb_common::Result<Ordering> {
            // No read counted, because a format that keeps the start of each value beside its rank
            // settles a probe without going near the payload, and that is the case being tested.
            Ok(self.values[self.order[rank] as usize].as_slice().cmp(wanted))
        }

        fn code_at_rank(&self, rank: usize) -> rudb_common::Result<u32> {
            Ok(self.order[rank])
        }
    }

    /// A dictionary column over values that came out of a file with their sorted order, beside the
    /// same rows written out flat so the two can be compared.
    fn filed(words: &[&str], codes: Vec<u32>) -> (Vector, Vector, Arc<Filed>) {
        let values: Vec<Vec<u8>> = words.iter().map(|text| text.as_bytes().to_vec()).collect();
        let mut order = (0..values.len() as u32).collect::<Vec<_>>();
        order.sort_by(|&left, &right| values[left as usize].cmp(&values[right as usize]));
        let source = Arc::new(Filed { values, order, reads: AtomicUsize::new(0) });
        let dictionary = Arc::new(
            Vector::external_text(LogicalType::Varchar, Arc::clone(&source) as Arc<_>)
                .expect("a filed vector"),
        );
        let flat = Vector::from_values(
            LogicalType::Varchar,
            &codes
                .iter()
                .map(|&code| Value::Varchar(words[code as usize].into()))
                .collect::<Vec<_>>(),
        )
        .expect("the same rows written out");
        let column = Vector::stable_dictionary(codes, dictionary).expect("codes are in range");
        (column, flat, source)
    }

    /// The case the searching path exists for. The list is found in the dictionary once for the
    /// whole query and the rows are then codes against codes, so the answer is the answer the flat
    /// column gives and the payload is never touched.
    #[test]
    fn a_sorted_dictionary_is_searched_once_and_no_value_is_read() {
        let (column, flat, source) =
            filed(&["AIR", "MAIL", "RAIL", "SHIP", "TRUCK"], vec![1, 0, 3, 4, 1, 2, 3]);
        let list = [Value::Varchar("MAIL".into()), Value::Varchar("SHIP".into())];
        assert_eq!(over(&column, &list, false), over(&flat, &list, false));
        assert_eq!(over(&column, &list, true), over(&flat, &list, true));
        assert_eq!(source.reads.load(Memory::Relaxed), 0);
    }

    /// A list the dictionary holds none of, which the search settles for the whole column without
    /// looking at a single code.
    #[test]
    fn a_list_the_dictionary_does_not_hold_is_false_everywhere() {
        let (column, flat, _) = filed(&["AIR", "MAIL", "SHIP"], vec![0, 1, 2, 1]);
        let list = [Value::Varchar("BOAT".into()), Value::Varchar("CART".into())];
        assert_eq!(over(&column, &list, false), over(&flat, &list, false));
        assert_eq!(over(&column, &list, false), vec![Value::Boolean(false); 4]);
    }

    /// Half in and half out, which is the case a search that stopped at the first miss would get
    /// wrong, and the nulls of the column on top of it.
    #[test]
    fn a_list_the_dictionary_holds_some_of_answers_what_the_flat_column_answers() {
        let (column, flat, _) = filed(&["AIR", "MAIL", "SHIP"], vec![0, 1, 2, 1, 0]);
        let list = [Value::Varchar("MAIL".into()), Value::Varchar("BOAT".into())];
        assert_eq!(over(&column, &list, false), over(&flat, &list, false));
        let holed = column
            .with_validity(rudb_vector::Validity::from_run(&[true, false, true, true, false]));
        let holed_flat =
            flat.with_validity(rudb_vector::Validity::from_run(&[true, false, true, true, false]));
        assert_eq!(over(&holed, &list, false), over(&holed_flat, &list, false));
    }

    /// A list of strings over a dictionary column, threaded, through both the sorted search and the
    /// memo a dictionary with no order goes through, against the flags the flat column answers.
    #[test]
    fn a_text_list_over_a_dictionary_selects_what_the_flat_column_flags() {
        let words = ["AIR", "MAIL", "RAIL", "SHIP", "TRUCK"];
        let codes: Vec<u32> = (0..300_u32).map(|row| (row * 7 + row / 3) % 5).collect();
        let (sorted, flat, source) = filed(&words, codes.clone());
        let values = Vector::from_values(
            LogicalType::Varchar,
            &words.iter().map(|&word| Value::Varchar(word.into())).collect::<Vec<_>>(),
        )
        .expect("builds");
        let unsorted =
            Vector::stable_dictionary(codes, Arc::new(values)).expect("codes are in range");
        let live = Selection::from_predicate(300, |row| row % 3 != 1);
        let lists = [
            vec![Value::Varchar("MAIL".into()), Value::Varchar("SHIP".into())],
            vec![Value::Varchar("BOAT".into()), Value::Varchar("CART".into())],
        ];
        for list in &lists {
            for negated in [false, true] {
                let yes = over(&flat, list, negated);
                let kept = |row: usize| yes[row] == Value::Boolean(true);
                let every = Selection::from_predicate(300, kept);
                let among = Selection::from_indices(
                    live.indices().iter().copied().filter(|&row| kept(row as usize)).collect(),
                );
                for column in [&sorted, &unsorted] {
                    // A fresh list each time, so each column starts with nothing remembered.
                    let members = Members::of(list, negated).expect("this list folds");
                    let some = select_in(column, &members, Some(&live)).expect("a dictionary");
                    assert_eq!(some, among, "{list:?} negated {negated} over the live rows");
                    let all = select_in(column, &members, None).expect("the same dictionary");
                    assert_eq!(all, every, "{list:?} negated {negated} over every row");
                }
            }
        }
        assert_eq!(source.reads.load(Memory::Relaxed), 0, "the sorted search read a value");
        // A null in the list is left to the flag kernel, which knows the rule.
        let nulled =
            Members::of(&[Value::Varchar("MAIL".into()), Value::Null], false).expect("folds");
        assert!(select_in(&sorted, &nulled, None).is_none());
    }

    /// No null in the column and none in the list, which is the path that writes only the answer.
    /// Every row has one, so what comes back carries no mask for a filter above it to walk.
    #[test]
    fn a_column_and_a_list_with_no_null_in_either_answer_every_row() {
        let (column, flat, _) = filed(&["AIR", "MAIL", "SHIP"], vec![0, 1, 2, 1, 0]);
        let list = [Value::Varchar("MAIL".into()), Value::Varchar("SHIP".into())];
        for negated in [false, true] {
            let members = Members::of(&list, negated).expect("this list folds");
            for input in [&column, &flat] {
                let answer =
                    in_set(input, &members, &LogicalType::Boolean).expect("the lookup runs");
                assert_eq!(
                    answer.validity().live(),
                    rudb_vector::Live::All,
                    "a row came back null"
                );
                let read: Vec<Value> = (0..5).map(|row| answer.value_at(row)).collect();
                let want =
                    [false, true, true, true, false].map(|hit| Value::Boolean(hit != negated));
                assert_eq!(read, want);
            }
        }
    }

    #[test]
    fn a_list_says_how_many_distinct_values_it_holds() {
        let list = [Value::Integer(1), Value::Integer(1), Value::Integer(2), Value::Null];
        let members = Members::of(&list, false).expect("this list folds");
        assert_eq!(members.len(), 2);
        assert!(!members.is_empty());
    }
}
