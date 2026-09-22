//! What one stored table looks like on disk, which is `pragma_storage_info(name)`.
//!
//! The only metadata table whose rows come out of the data rather than out of the catalog or out of
//! a list this binary was compiled with. Everything the other ones report is in memory before the
//! query starts. This one opens the column pages of the table it was pointed at and reads the
//! header of every part of every column, because what the encoder chose is written there and
//! nowhere else.
//!
//! That is the whole reason to have it. The directory says a column took 41 megabytes and says
//! nothing about what shape they are in, and the shape is what a scan pays for. Two files holding
//! the same six million rows in a different order come back bit packed on one and plain on the
//! other, and until this existed there was no way to find out which.
//!
//! The columns are DuckDB's sixteen, in DuckDB's order and types, measured against the pin rather
//! than read off the documentation. `rudb_functions::table::storage_info_fields` has the list and
//! says what each one means here, because the words are DuckDB's and the storage is ours.
//!
//! A table with nothing written down yet produces no rows. Rows that are still in memory have no
//! encoding to report, since the encoder has not run on them and will not until a checkpoint, so
//! the honest answer is to leave them out rather than to claim they are stored plain.

use rudb_catalog::{Catalog, StoredPart};
use rudb_common::{Result, Value};
use rudb_functions::table::storage_info_fields;
use rudb_parse::identifier_parts;
use rudb_plan::{Plan, Slice};

use crate::metadata::{Metadata, text};

/// Every stored part of every column of one table, in the columns the plan asked for.
///
/// The column order is the outer loop and the parts are the inner one, which is the pin's order and
/// is also the order the pages sit in the file, so a reader looking at one column's rows sees them
/// together.
///
/// # Errors
///
/// If the name does not resolve to a table, if a page or checksum the reader has to open is
/// invalid, or if the plan asks for a column this table does not have.
pub(crate) fn storage_info(
    catalog: &Catalog,
    written: &str,
    plan: &Plan,
    index: u32,
    columns: Slice,
) -> Result<Metadata> {
    let parts = identifier_parts(written);
    let spelled: Vec<&str> = parts.iter().map(String::as_str).collect();
    let name = catalog.resolve(&spelled)?;
    let table = catalog.table(&name)?;
    let mut rows = Vec::new();
    for (at, field) in table.columns().iter().enumerate() {
        let column = i64::try_from(at).unwrap_or(i64::MAX);
        for stored in table.stored(at)? {
            rows.push(vec![
                Value::BigInt(i64::try_from(stored.stripe).unwrap_or(i64::MAX)),
                text(&field.name),
                Value::BigInt(column),
                text(&format!("[{column}]")),
                Value::BigInt(i64::try_from(stored.part).unwrap_or(i64::MAX)),
                text(&field.ty.to_string()),
                Value::BigInt(i64::try_from(stored.row).unwrap_or(i64::MAX)),
                Value::BigInt(i64::try_from(stored.rows).unwrap_or(i64::MAX)),
                text(&stored.encoding),
                text(&stats(&stored)),
                Value::Boolean(false),
                Value::Boolean(true),
                Value::BigInt(i64::try_from(stored.page).unwrap_or(i64::MAX)),
                Value::BigInt(i64::try_from(stored.offset).unwrap_or(i64::MAX)),
                text(&format!("{} bytes", stored.bytes)),
                Value::List { element: rudb_common::LogicalType::BigInt, values: Vec::new() },
            ]);
        }
    }
    Metadata::new("pragma_storage_info", &storage_info_fields(), &rows, plan, index, columns)
}

/// The two ends and the null count of one part, written the way the pin writes them.
///
/// One string with two bracketed halves rather than four columns, which is upstream's choice and
/// not a good one, but a client that parses it parses the same shape here. A part whose column has
/// no ordered bound, which is every column of a type the range walk cannot compare, says so rather
/// than printing an empty pair.
fn stats(stored: &StoredPart) -> String {
    let (Some(low), Some(high), Some(nulls)) = (&stored.low, &stored.high, stored.nulls) else {
        return "[No Stats]".to_string();
    };
    format!(
        "[Min: {low}, Max: {high}][Has Null: {}, Has No Null: {}]",
        nulls > 0,
        nulls < stored.rows
    )
}
