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
use rudb_common::{Clustering, Error, Field, Result, Value, Width};
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
    let columns = clustering.columns().iter().map(|&at| at as usize).collect::<Vec<_>>();
    let mut keys = keys(rows, &columns)?;
    let width = match clustering.width() {
        Width::Auto => {
            let days = keys.iter().filter_map(|key| day(&key[0])).fold(None, |span, day| {
                let (low, high) = span.unwrap_or((day, day));
                Some((low.min(day), high.max(day)))
            });
            let days = days.map_or(0, |(low, high)| u64::try_from(high - low).unwrap_or(0));
            clustering.fitted(keys.len() as u64, days).width()
        }
        width => width,
    };
    if width != Width::Exact {
        for key in &mut keys {
            if let Some(day) = day(&key[0]) {
                key[0] = Value::Date(bucket(day, width));
            }
        }
    }
    let mut order = numbered(keys.len())?;
    order.sort_by(|&left, &right| compare(&keys[left as usize], &keys[right as usize]));
    Ok(order)
}

/// The rows ordered by where the parent row each one matches will be stored, then by their own
/// place. A row that matches no parent row goes after every row that does.
///
/// The parent's keys are sorted once and each child row finds its parent by a binary search, which
/// needs nothing of a value beyond the ordering every type already has.
fn through(
    rows: &Rows,
    own: &[usize],
    parent: &Rows,
    theirs: &[usize],
    moved: Option<&[u32]>,
) -> Result<Vec<u32>> {
    let parents = keys(parent, theirs)?;
    // Where each parent row is written. With no new order that is where it is now.
    let mut placed = numbered(parents.len())?;
    if let Some(moved) = moved {
        for (to, &from) in moved.iter().enumerate() {
            placed[from as usize] = to as u32;
        }
    }
    let mut sorted = numbered(parents.len())?;
    sorted.sort_by(|&left, &right| compare(&parents[left as usize], &parents[right as usize]));
    let children = keys(rows, own)?;
    let unmatched = u32::MAX;
    // row at a time: once per row of a checkpoint that is putting the table in a new order, and the
    // search compares values the sort above already has in hand.
    let at = children
        .iter()
        .map(|key| {
            if key.iter().any(Value::is_null) {
                return unmatched;
            }
            sorted
                .binary_search_by(|&probe| compare(&parents[probe as usize], key))
                .map_or(unmatched, |found| placed[sorted[found] as usize])
        })
        .collect::<Vec<_>>();
    let mut order = numbered(children.len())?;
    order.sort_by_key(|&row| at[row as usize]);
    Ok(order)
}

/// Row numbers `0..rows`, which a table past four billion rows cannot be counted in.
fn numbered(rows: usize) -> Result<Vec<u32>> {
    let rows =
        u32::try_from(rows).map_err(|_| Error::internal("a table past four billion rows"))?;
    Ok((0..rows).collect())
}

/// The values of `columns` for every row, in the order the table reads them.
fn keys(rows: &Rows, columns: &[usize]) -> Result<Vec<Vec<Value>>> {
    let mut keys = Vec::with_capacity(rows.len());
    for at in 0..rows.chunk_count() {
        let chunk = rows.read(at, columns)?;
        let vectors =
            (0..columns.len()).map(|index| chunk.column(index)).collect::<Result<Vec<_>>>()?;
        // row at a time: a sort key per row, built once per checkpoint that reorders the table,
        // because the sort compares rows across chunks and a chunk's vectors go when it does.
        for row in 0..chunk.len() {
            keys.push(
                vectors.iter().map(|vector| vector.try_value_at(row)).collect::<Result<_>>()?,
            );
        }
    }
    Ok(keys)
}

/// Two keys in ascending order with nulls last, the way the declaration's sort puts them.
fn compare(left: &[Value], right: &[Value]) -> Ordering {
    for (left, right) in left.iter().zip(right) {
        let ordering = match (left.is_null(), right.is_null()) {
            (true, true) => Ordering::Equal,
            (true, false) => Ordering::Greater,
            (false, true) => Ordering::Less,
            // Two values of one column always compare, so an error here is a column of two types,
            // and calling them equal leaves the rows in the order they came in.
            (false, false) => rudb_kernels::order(left, right).unwrap_or(Ordering::Equal),
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
    use super::{Width, bucket};
    use rudb_common::value::days_from_civil;

    #[test]
    fn a_day_falls_in_the_bucket_that_starts_its_month_quarter_and_year() {
        let day = days_from_civil(1995, 8, 17);
        assert_eq!(bucket(day, Width::Month), days_from_civil(1995, 8, 1));
        assert_eq!(bucket(day, Width::Quarter), days_from_civil(1995, 7, 1));
        assert_eq!(bucket(day, Width::Year), days_from_civil(1995, 1, 1));
        assert_eq!(bucket(day, Width::Exact), day);
    }
}
