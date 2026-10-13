//! The order a checkpoint writes a table's rows in, when the table declared one.
//!
//! A declaration is `cluster_by`, and it comes in two forms. One orders a table by its own columns,
//! the leading one bucketed at a calendar width, which is what an `INSERT` already sorts its own
//! rows by. The other orders a table through its parent: by where the parent row each row points at
//! is stored, then by the row's own place. That is the layout of section 12.2 of
//! `spec/graph/12-the-order-the-suite-asks-for.md`, `lineitem` through `orders` with `orders` in
//! date order, which keeps every order's lines together and puts them in date order. An insert
//! cannot sort for the second form, because where the parent's rows end up is decided here.
//!
//! So this is where both are honoured for a table written whole. A table carried forward or
//! extended keeps the order its stripes already have, and a table whose file already records its
//! declaration is left in the order it was written in, which is the order it was put in last time.
//! That is what keeps a checkpoint of a database where nothing moved from sorting it again.
//!
//! The rows are put in order in memory. A base load is the case this is for and its rows are in
//! memory already, so the sort costs a permutation of row numbers and a gather of each column a
//! window at a time, and nothing spills.

use std::cmp::Ordering;
use std::sync::Arc;

use rudb_catalog::{Catalog, QualifiedName, Rows};
use rudb_common::value::{civil_from_days, days_from_civil};
use rudb_common::{Clustering, Error, Field, LogicalType, Result, Value, Width};
use rudb_vector::{Chunk, Vector};

/// Rows a part holds, the same number the plain write cuts at.
const PART_ROWS: usize = 8_192;

/// Rows of the order gathered at once, sixteen parts.
const WINDOW_ROWS: usize = PART_ROWS * 16;

/// Microseconds in a day, for a timestamp bucketed by its date.
const DAY_MICROS: i64 = 86_400_000_000;

/// The orders worked out so far in one checkpoint, so a parent sorted for its own write is not
/// sorted again for each child ordered through it.
#[derive(Default)]
pub(crate) struct Orders {
    known: Vec<(QualifiedName, Option<Arc<Vec<u32>>>)>,
}

impl Orders {
    /// The rows of `name` in the order they are to be written, as positions into the order the
    /// table reads them in, or `None` for the order they already have.
    ///
    /// `whole` says whether a table is written whole by this checkpoint, which is the only kind of
    /// write that can put a table's rows in a new order. A parent that is not keeps its stored order
    /// and its children are ordered by that.
    pub(crate) fn of(
        &mut self,
        catalog: &Catalog,
        name: &QualifiedName,
        whole: &dyn Fn(&QualifiedName) -> bool,
    ) -> Result<Option<Arc<Vec<u32>>>> {
        if let Some(known) = self.known(name) {
            return Ok(known);
        }
        let worked = self.work_out(catalog, name, whole, 0)?;
        self.known.push((name.clone(), worked.clone()));
        Ok(worked)
    }

    /// The order already worked out for `name`, if it was. A list and not a map, since a database
    /// declares an order on a handful of tables and a name has no ordering of its own.
    fn known(&self, name: &QualifiedName) -> Option<Option<Arc<Vec<u32>>>> {
        self.known.iter().find(|(known, _)| known == name).map(|(_, order)| order.clone())
    }

    fn work_out(
        &mut self,
        catalog: &Catalog,
        name: &QualifiedName,
        whole: &dyn Fn(&QualifiedName) -> bool,
        depth: usize,
    ) -> Result<Option<Arc<Vec<u32>>>> {
        let table = catalog.table(name)?;
        let Some(clustering) = table.clustering() else { return Ok(None) };
        if !whole(name) || table.rows().is_empty() {
            return Ok(None);
        }
        let Some(parent) = clustering.parent() else {
            if table.clustering_is_stored() {
                return Ok(None);
            }
            return Ok(Some(Arc::new(by_own(table.rows(), clustering)?)));
        };
        // A parent dropped since the declaration orders nothing, and neither does a chain of
        // parents that comes back round on itself, which the setting cannot stop since each step
        // of it is a separate entry.
        let Ok(parent_name) =
            catalog.resolve(&[name.catalog.as_str(), name.schema.as_str(), parent.table()])
        else {
            return Ok(None);
        };
        if depth > 8 {
            return Ok(None);
        }
        let moved = match self.known(&parent_name) {
            Some(known) => known,
            None => {
                let worked = self.work_out(catalog, &parent_name, whole, depth + 1)?;
                self.known.push((parent_name.clone(), worked.clone()));
                worked
            }
        };
        if moved.is_none() && table.clustering_is_stored() {
            return Ok(None);
        }
        let parent_table = catalog.table(&parent_name)?;
        let mut theirs = Vec::with_capacity(parent.columns().len());
        for column in parent.columns() {
            let Some(at) = parent_table
                .columns()
                .iter()
                .position(|field| field.name.eq_ignore_ascii_case(column))
            else {
                return Ok(None);
            };
            theirs.push(at);
        }
        let own = clustering.columns().iter().map(|&at| at as usize).collect::<Vec<_>>();
        let order = through(
            table.rows(),
            &own,
            parent_table.rows(),
            &theirs,
            moved.as_ref().map(|order| order.as_slice()),
        )?;
        Ok(Some(Arc::new(order)))
    }
}

/// The rows ordered by the table's own columns: the leading one bucketed at the declared width, nulls
/// last, and ties left in the order they came in.
fn by_own(rows: &Rows, clustering: &Clustering) -> Result<Vec<u32>> {
    let mut keys = clustering
        .columns()
        .iter()
        .map(|&at| Key::read(rows, at as usize))
        .collect::<Result<Vec<_>>>()?;
    let width = match clustering.width() {
        Width::Auto => clustering.fitted(rows.len() as u64, keys[0].span()).width(),
        width => width,
    };
    if width != Width::Exact {
        keys[0].bucket(width);
    }
    let mut order = numbered(rows.len())?;
    order.sort_by(|&left, &right| compare(&keys, left as usize, &keys, right as usize));
    Ok(order)
}

/// The rows ordered by where the parent row each one matches will be stored, then by their own
/// place. A row that matches no parent row goes after every row that does, and a row whose key more
/// than one parent row holds goes with the one written first.
///
/// One integer column on each side is a foreign key nearly always, and it is matched through the
/// parent's keys sorted beside where each is written, a plain array a search runs down. Any other
/// key sorts the parent's row numbers by the key and searches those, which needs nothing of a value
/// beyond the ordering every type already has. Either way the rows are then dealt out by where
/// their parents are written, which is a count and not a sort, since a place is a row number.
fn through(
    rows: &Rows,
    own: &[usize],
    parent: &Rows,
    theirs: &[usize],
    moved: Option<&[u32]>,
) -> Result<Vec<u32>> {
    let mut parents = theirs.iter().map(|&at| Key::read(parent, at)).collect::<Result<Vec<_>>>()?;
    let mut children = own.iter().map(|&at| Key::read(rows, at)).collect::<Result<Vec<_>>>()?;
    // The two sides have one type, but a side can still come back as values where the other came
    // back as integers, from a chunk held in a form that has no block, and the two are compared in
    // one form.
    for (index, (child, parent_key)) in children.iter_mut().zip(&mut parents).enumerate() {
        if child.is_signed() != parent_key.is_signed() {
            *child = Key::values(rows, own[index])?;
            *parent_key = Key::values(parent, theirs[index])?;
        }
    }
    // Where each parent row is written. With no new order that is where it is now.
    let mut placed = numbered(parent.len())?;
    if let Some(moved) = moved {
        for (to, &from) in moved.iter().enumerate() {
            placed[from as usize] = to as u32;
        }
    }
    let unmatched = u32::MAX;
    let at = match (parents.as_slice(), children.as_slice()) {
        (
            [Key::Signed { values: keys, nulls: gone, .. }],
            [Key::Signed { values: wanted, nulls: missing, .. }],
        ) => {
            let mut sorted = keys
                .iter()
                .zip(gone)
                .zip(&placed)
                .filter(|((_, null), _)| !**null)
                .map(|((&key, _), &place)| (key, place))
                .collect::<Vec<_>>();
            sorted.sort_unstable();
            sorted.dedup_by_key(|(key, _)| *key);
            wanted
                .iter()
                .zip(missing)
                .map(|(key, &null)| {
                    if null {
                        return unmatched;
                    }
                    sorted
                        .binary_search_by_key(key, |&(key, _)| key)
                        .map_or(unmatched, |found| sorted[found].1)
                })
                .collect::<Vec<_>>()
        }
        _ => {
            let mut sorted = numbered(parent.len())?;
            // Ties by where the row is written, so a key more than one parent row holds is found
            // at the one written first, the same one the integer match finds.
            sorted.sort_by(|&left, &right| {
                compare(&parents, left as usize, &parents, right as usize)
                    .then(placed[left as usize].cmp(&placed[right as usize]))
            });
            (0..rows.len())
                .map(|row| {
                    if children.iter().any(|key| key.is_null(row)) {
                        return unmatched;
                    }
                    let first = sorted.partition_point(|&probe| {
                        compare(&parents, probe as usize, &children, row) == Ordering::Less
                    });
                    match sorted.get(first) {
                        Some(&found)
                            if compare(&parents, found as usize, &children, row)
                                == Ordering::Equal =>
                        {
                            placed[found as usize]
                        }
                        _ => unmatched,
                    }
                })
                .collect::<Vec<_>>()
        }
    };
    dealt(&at, parent.len())
}

/// Row numbers in the order of the places `at` gives them, a place past `places` last, and rows
/// with one place in the order they came in.
fn dealt(at: &[u32], places: usize) -> Result<Vec<u32>> {
    u32::try_from(at.len()).map_err(|_| Error::internal("a table past four billion rows"))?;
    let slot = |place: u32| (place as usize).min(places);
    let mut starts = vec![0_usize; places + 2];
    for &place in at {
        starts[slot(place) + 1] += 1;
    }
    for index in 1..starts.len() {
        starts[index] += starts[index - 1];
    }
    let mut order = vec![0_u32; at.len()];
    for (row, &place) in at.iter().enumerate() {
        let next = &mut starts[slot(place)];
        order[*next] = row as u32;
        *next += 1;
    }
    Ok(order)
}

/// Row numbers `0..rows`, which a table past four billion rows cannot be counted in.
fn numbered(rows: usize) -> Result<Vec<u32>> {
    let rows =
        u32::try_from(rows).map_err(|_| Error::internal("a table past four billion rows"))?;
    Ok((0..rows).collect())
}

/// One column of a sort key, the whole table of it, in the order the table reads its rows.
///
/// A key is compared a column at a time by row number, so a row's key is never put together in one
/// place, and an integer, date or timestamp column is held as the integers it is stored as. Any
/// other column is held as values.
enum Key {
    Signed { values: Vec<i64>, nulls: Vec<bool>, ty: LogicalType },
    Values(Vec<Value>),
}

impl Key {
    /// Column `column` of `rows`, as integers when every chunk of it hands its rows over as a
    /// block and as values when one does not.
    fn read(rows: &Rows, column: usize) -> Result<Self> {
        let mut values = Vec::with_capacity(rows.len());
        let mut nulls = Vec::with_capacity(rows.len());
        let mut block = Vec::new();
        let mut ty = None;
        for at in 0..rows.chunk_count() {
            let chunk = rows.read(at, &[column])?;
            let vector = chunk.column(0)?;
            if !vector.signed_block(&mut block) {
                return Self::values(rows, column);
            }
            ty.get_or_insert_with(|| vector.logical_type().clone());
            let len = vector.len();
            if vector.validity().has_nulls(len) {
                nulls.extend((0..len).map(|row| vector.is_null_at(row)));
            } else {
                nulls.resize(nulls.len() + len, false);
            }
            values.extend_from_slice(&block);
        }
        Ok(Self::Signed { values, nulls, ty: ty.unwrap_or(LogicalType::BigInt) })
    }

    /// Column `column` of `rows` as values.
    fn values(rows: &Rows, column: usize) -> Result<Self> {
        let mut values = Vec::with_capacity(rows.len());
        for at in 0..rows.chunk_count() {
            let chunk = rows.read(at, &[column])?;
            let vector = chunk.column(0)?;
            // row at a time: the key of a column with no integer form, a string or a float, read
            // once per checkpoint that reorders the table, because the sort compares rows across
            // chunks and a chunk's vectors go when it does.
            for row in 0..vector.len() {
                values.push(vector.try_value_at(row)?);
            }
        }
        Ok(Self::Values(values))
    }

    fn is_signed(&self) -> bool {
        matches!(self, Self::Signed { .. })
    }

    fn is_null(&self, row: usize) -> bool {
        match self {
            Self::Signed { nulls, .. } => nulls[row],
            Self::Values(values) => values[row].is_null(),
        }
    }

    /// The day row `row` falls on, for a date or a timestamp that is not null.
    fn day(&self, row: usize) -> Option<i32> {
        match self {
            Self::Signed { values, nulls, ty } if !nulls[row] => match ty {
                LogicalType::Date => i32::try_from(values[row]).ok(),
                LogicalType::Timestamp => i32::try_from(values[row].div_euclid(DAY_MICROS)).ok(),
                _ => None,
            },
            Self::Signed { .. } => None,
            Self::Values(values) => day(&values[row]),
        }
    }

    /// The days between the first and the last the column falls on.
    fn span(&self) -> u64 {
        let rows = match self {
            Self::Signed { values, .. } => values.len(),
            Self::Values(values) => values.len(),
        };
        let days = (0..rows).filter_map(|row| self.day(row)).fold(None, |span, day| {
            let (low, high) = span.unwrap_or((day, day));
            Some((low.min(day), high.max(day)))
        });
        days.map_or(0, |(low, high)| u64::try_from(high - low).unwrap_or(0))
    }

    /// Every date or timestamp in the column put on the first day of its bucket at `width`.
    fn bucket(&mut self, width: Width) {
        match self {
            Self::Signed { values, nulls, ty } => {
                let date = *ty == LogicalType::Date;
                if !date && *ty != LogicalType::Timestamp {
                    return;
                }
                for (value, &null) in values.iter_mut().zip(nulls.iter()) {
                    let day = if date { *value } else { value.div_euclid(DAY_MICROS) };
                    if let (false, Ok(day)) = (null, i32::try_from(day)) {
                        *value = i64::from(bucket(day, width));
                    }
                }
                *ty = LogicalType::Date;
            }
            Self::Values(values) => {
                // row at a time: the bucket of a key held as values, which a date or a timestamp
                // column is only when one of its chunks had no integer form.
                for value in values.iter_mut() {
                    if let Some(day) = day(value) {
                        *value = Value::Date(bucket(day, width));
                    }
                }
            }
        }
    }
}

/// Row `left` of the key `lefts` against row `right` of the key `rights`, in ascending order with
/// nulls last, the way the declaration's sort puts them. The two are one key or a child's key and
/// its parent's, whose columns are of one type and held in one form.
fn compare(lefts: &[Key], left: usize, rights: &[Key], right: usize) -> Ordering {
    for (one, other) in lefts.iter().zip(rights) {
        let ordering = match (one.is_null(left), other.is_null(right)) {
            (true, true) => Ordering::Equal,
            (true, false) => Ordering::Greater,
            (false, true) => Ordering::Less,
            (false, false) => match (one, other) {
                (Key::Signed { values: ones, .. }, Key::Signed { values: others, .. }) => {
                    ones[left].cmp(&others[right])
                }
                // Two values of one column always compare, so an error here is a column of two
                // types, and calling them equal leaves the rows in the order they came in.
                (Key::Values(ones), Key::Values(others)) => {
                    rudb_kernels::order(&ones[left], &others[right]).unwrap_or(Ordering::Equal)
                }
                _ => Ordering::Equal,
            },
        };
        if ordering != Ordering::Equal {
            return ordering;
        }
    }
    Ordering::Equal
}

/// The day a date or a timestamp falls on, for the bucket and the span.
fn day(value: &Value) -> Option<i32> {
    match *value {
        Value::Date(day) => Some(day),
        Value::Timestamp(micros) | Value::TimestampTz(micros) => {
            i32::try_from(micros.div_euclid(DAY_MICROS)).ok()
        }
        _ => None,
    }
}

/// The first day of the month, quarter or year a day falls in.
fn bucket(day: i32, width: Width) -> i32 {
    let (year, month, _) = civil_from_days(day);
    match width {
        Width::Month => days_from_civil(year, month, 1),
        Width::Quarter => days_from_civil(year, (month - 1) / 3 * 3 + 1, 1),
        Width::Year => days_from_civil(year, 1, 1),
        Width::Exact | Width::Auto => day,
    }
}

/// Writes every row of `rows` in `order`, a part at a time.
///
/// A window of parts at a time, and gathered out of the chunks the rows are held in rather than out
/// of each column laid end to end. Laying the columns out first is the simple way and it costs a
/// second copy of the whole table on top of the one being written from: the SF1 `lineitem` through
/// `orders` checkpoint peaked past 2.9 GB that way and was killed, where loading the same rows in
/// the same order through a join and a sort peaked at 2.1 GB. A window is [`WINDOW_ROWS`] of the
/// order. Its rows are looked up a chunk at a time in chunk order, so each chunk is read once a
/// window and every column of it gathered at once, and the pieces are put back into the order's
/// order with one more gather. What is held besides the table is a window, not a table.
pub(crate) fn write(
    open: &mut rudb_native::Writer,
    rows: &Rows,
    order: &[u32],
    fields: &[Field],
) -> Result<()> {
    let every = (0..fields.len()).collect::<Vec<_>>();
    let mut starts = Vec::with_capacity(rows.chunk_count() + 1);
    let mut at = 0usize;
    for chunk in 0..rows.chunk_count() {
        starts.push(at);
        at += rows.chunk_len(chunk)?;
    }
    starts.push(at);
    for window in order.chunks(WINDOW_ROWS) {
        let mut by = (0..window.len()).collect::<Vec<_>>();
        by.sort_unstable_by_key(|&slot| window[slot]);
        let mut back = vec![0u32; window.len()];
        let mut pieces: Vec<Vec<Vector>> = fields.iter().map(|_| Vec::new()).collect();
        let mut next = 0;
        while next < by.len() {
            let row = window[by[next]] as usize;
            let chunk = starts.partition_point(|&start| start <= row) - 1;
            let end = starts[chunk + 1];
            let mut local = Vec::new();
            while next < by.len() && (window[by[next]] as usize) < end {
                back[by[next]] = u32::try_from(next)
                    .map_err(|_| Error::internal("a window past what a row number holds"))?;
                local.push(
                    u32::try_from(window[by[next]] as usize - starts[chunk])
                        .map_err(|_| Error::internal("a chunk past what a row number holds"))?,
                );
                next += 1;
            }
            let read = rows.read(chunk, &every)?;
            for (column, kept) in pieces.iter_mut().enumerate() {
                kept.push(read.column(column)?.gather(&local)?);
            }
        }
        let mut columns = Vec::with_capacity(fields.len());
        for (field, kept) in fields.iter().zip(&pieces) {
            columns.push(laid(field, &kept.iter().collect::<Vec<_>>())?);
        }
        drop(pieces);
        for part in back.chunks(PART_ROWS) {
            let gathered =
                columns.iter().map(|column| column.gather(part)).collect::<Result<Vec<_>>>()?;
            open.append(&Chunk::with_rows(gathered, part.len())?)?;
        }
    }
    Ok(())
}

/// One column's pieces laid end to end as one vector.
fn laid(field: &Field, pieces: &[&Vector]) -> Result<Vector> {
    if let Ok(Some(vector)) = rudb_vector::concat(&field.ty, pieces) {
        return Ok(vector);
    }
    let mut values = Vec::with_capacity(pieces.iter().map(|piece| piece.len()).sum());
    for vector in pieces {
        // row at a time: the fallback for a type the typed concatenation does not lay out, the
        // same one the plain write takes.
        for row in 0..vector.len() {
            values.push(vector.try_value_at(row)?);
        }
    }
    Vector::from_values(field.ty.clone(), &values)
}

#[cfg(test)]
mod tests {
    use super::{Width, bucket, dealt};
    use rudb_common::value::days_from_civil;

    #[test]
    fn a_day_falls_in_the_bucket_that_starts_its_month_quarter_and_year() {
        let day = days_from_civil(1995, 8, 17);
        assert_eq!(bucket(day, Width::Month), days_from_civil(1995, 8, 1));
        assert_eq!(bucket(day, Width::Quarter), days_from_civil(1995, 7, 1));
        assert_eq!(bucket(day, Width::Year), days_from_civil(1995, 1, 1));
        assert_eq!(bucket(day, Width::Exact), day);
    }

    #[test]
    fn rows_are_dealt_by_place_with_ties_in_the_order_they_came_and_no_place_last() {
        let unmatched = u32::MAX;
        let order = dealt(&[2, unmatched, 0, 2, 1, unmatched, 0], 3).expect("deals");
        assert_eq!(order, [2, 6, 4, 0, 3, 1, 5]);
    }
}
