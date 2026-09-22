//! How many rows of a table in memory hold each value of a column, for the planner to ask.
//!
//! The file side of this is `rudb_native::Common`, which answers out of the synopsis the writer put
//! in the directory and can keep answering for as long as the reader is open. A table in memory has
//! no directory and no reader. What it has is the tally `rudb_storage::tally` built as the rows
//! arrived, and that tally belongs to the table rather than being shared behind a reference count,
//! so it cannot be handed to a plan the way a file's can.
//!
//! So this is a copy, taken once when the statement is bound. That is affordable here and only here,
//! because of the one property the cap in `tally.rs` gives the lists: a column either holds at most
//! five hundred and twelve values or holds no list at all. A copy of the zone maps of a table in
//! memory would be a copy of a summary per chunk and would grow with the rows, which is why
//! `Rows::zones` still answers `None` for one. A copy of the frequency lists is bounded by the
//! schema.
//!
//! The names come from the catalog entry, because a table in memory keeps its types and not its
//! names. That is the same reason `Table::distincts` is on the table rather than on the rows.

use std::cmp::Ordering;

use rudb_common::bounds::{Bound, Frequencies, Remainder};
use rudb_common::stat::{Provenance, Stat};
use rudb_common::{Field, Value};
use rudb_storage::MemoryTable;

/// One column's values with the rows holding each, or nothing when the column keeps no list.
type Listed = Option<Vec<(Value, u64)>>;

/// The frequency lists of a table in memory, copied out of it and named.
#[derive(Debug, Clone)]
pub struct Held {
    /// One entry per column of the table, and `None` for a column with no list.
    ///
    /// Per column and not only for the columns that have one, so that a name resolves to the index
    /// the plan uses whether or not there is anything to say about it.
    columns: Vec<(String, Listed)>,
    rows: u64,
}

impl Held {
    /// The lists of one table, or `None` when not one column of it has one.
    ///
    /// `None` rather than an empty answer, because the planner reads the absence of this as a table
    /// that keeps no synopsis and an empty one as a table whose every column holds no values.
    #[must_use]
    pub fn of(rows: &MemoryTable, fields: &[Field]) -> Option<Self> {
        let mut any = false;
        let columns: Vec<(String, Listed)> = fields
            .iter()
            .enumerate()
            .map(|(at, field)| {
                // Asked before it is built, because a column with no list is the common case for a
                // wide table and the question is a comparison where the answer is a copy.
                let held = match rows.frequency_values(at) {
                    Some(_) => rows.frequencies(at).ok().flatten(),
                    None => None,
                };
                any |= held.is_some();
                (field.name.clone(), held)
            })
            .collect();
        any.then_some(Self { columns, rows: rows.len() as u64 })
    }
}

impl Frequencies for Held {
    fn column(&self, name: &str) -> Option<usize> {
        self.columns.iter().position(|(held, _)| held == name)
    }

    fn rows(&self) -> u64 {
        self.rows
    }

    fn rows_with(&self, column: usize, value: &Bound) -> Stat<u64> {
        let Some((_, Some(held))) = self.columns.get(column) else {
            return Stat::Unknown;
        };
        let mut comparable = false;
        for (entry, count) in held {
            // A null entry is the column's nulls and no equality matches a null, so it is skipped.
            // It is in the list all the same, because the rows under it are rows the values do not
            // account for and a caller subtracting from the total has to be able to see them.
            let Some(bound) = Bound::of_value(entry) else {
                continue;
            };
            match bound.order(value) {
                Some(Ordering::Equal) => return Stat::exact(*count, Provenance::FrequencySynopsis),
                Some(_) => comparable = true,
                None => {}
            }
        }
        // The list is every value the column holds, so a value that is not in it is in no row. Only
        // where something in the list would at least compare: a constant of another type would come
        // back as zero rows for a reason that is about the types rather than about the column.
        if comparable { Stat::exact(0, Provenance::FrequencySynopsis) } else { Stat::Unknown }
    }

    fn remainder(&self, _column: usize) -> Option<Remainder> {
        // Never. A list here is complete or it is absent, so there is nothing outside one for a
        // caller to divide the rows of. `tally.rs` has why a streaming pass cannot produce the other
        // kind: the counters it would end with are lower bounds, and every reader of this reads the
        // counts as exact.
        None
    }
}
