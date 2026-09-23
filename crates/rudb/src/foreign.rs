//! The two checks a write makes for foreign keys, one from each end.
//!
//! A row written into a table with a foreign key has to find its key in the referenced table, and
//! a write to a referenced table cannot take away a key some row still points at. A key with a null
//! in it points at nothing, so it passes the first check and never holds up the second.
//!
//! Both read the tables as they are before the write lands, which is what the pin does: a row that
//! references a row the same statement inserts is refused, even in a table that references itself.

use std::collections::HashSet;

use rudb_catalog::{Catalog, ForeignKey, Key, QualifiedName, Table};
use rudb_common::{Error, Result, Value};
use rudb_vector::Chunk;

/// The encoded keys over these columns of every row the table holds.
fn held(table: &Table, columns: &[usize]) -> Result<HashSet<Box<[u8]>>> {
    let key = Key { columns: (0..columns.len()).collect(), primary: false };
    let mut keys = HashSet::new();
    for at in 0..table.rows().chunk_count() {
        let chunk = table.rows().read(at, columns)?;
        for row in 0..chunk.len() {
            let values: Vec<Value> = chunk.row(row).collect();
            if let Some(encoded) = key.of_row(&values) {
                keys.insert(encoded);
            }
        }
    }
    Ok(keys)
}

/// `a: 1, b: x`, the columns of a key with the values one row has in them.
fn named(names: impl Iterator<Item = String>, values: impl Iterator<Item = Value>) -> String {
    names.zip(values).map(|(name, value)| format!("{name}: {value}")).collect::<Vec<_>>().join(", ")
}

/// Refuses rows about to be written into a table when one of them has a key its foreign key does
/// not find in the referenced table.
pub(crate) fn missing(catalog: &Catalog, name: &QualifiedName, rows: &[Chunk]) -> Result<()> {
    let table = catalog.table(name)?;
    for foreign in table.foreign() {
        let target = catalog.table(&foreign.table)?;
        let keys = held(target, &foreign.referenced)?;
        let key = Key { columns: foreign.columns.clone(), primary: false };
        for chunk in rows {
            for row in 0..chunk.len() {
                let values: Vec<Value> = chunk.row(row).collect();
                let Some(encoded) = key.of_row(&values) else { continue };
                if keys.contains(&encoded) {
                    continue;
                }
                let names = foreign.referenced.iter().map(|&at| target.columns()[at].name.clone());
                let values = foreign.columns.iter().map(|&at| values[at].clone());
                return Err(Error::constraint(format!(
                    "Violates foreign key constraint because key \"{}\" does not exist in the \
                     referenced table",
                    named(names, values)
                )));
            }
        }
    }
    Ok(())
}

/// Refuses a write that leaves a table holding these rows when a key it held before is gone and a
/// row of some table still references it.
pub(crate) fn lost(catalog: &Catalog, name: &QualifiedName, after: &[Chunk]) -> Result<()> {
    let holders: Vec<(&Table, &ForeignKey)> = catalog
        .tables()
        .flat_map(|table| {
            table.foreign().iter().filter(|foreign| &foreign.table == name).map(move |f| (table, f))
        })
        .collect();
    if holders.is_empty() {
        return Ok(());
    }
    let table = catalog.table(name)?;
    for (holder, foreign) in holders {
        let before = held(table, &foreign.referenced)?;
        let key = Key { columns: foreign.referenced.clone(), primary: false };
        let mut kept = HashSet::new();
        for chunk in after {
            for row in 0..chunk.len() {
                let values: Vec<Value> = chunk.row(row).collect();
                if let Some(encoded) = key.of_row(&values) {
                    kept.insert(encoded);
                }
            }
        }
        let gone: HashSet<&Box<[u8]>> = before.difference(&kept).collect();
        if gone.is_empty() {
            continue;
        }
        let key = Key { columns: (0..foreign.columns.len()).collect(), primary: false };
        for at in 0..holder.rows().chunk_count() {
            let chunk = holder.rows().read(at, &foreign.columns)?;
            for row in 0..chunk.len() {
                let values: Vec<Value> = chunk.row(row).collect();
                let Some(encoded) = key.of_row(&values) else { continue };
                if !gone.contains(&encoded) {
                    continue;
                }
                let names = foreign.columns.iter().map(|&at| holder.columns()[at].name.clone());
                return Err(Error::constraint(format!(
                    "Violates foreign key constraint because key \"{}\" is still referenced by a \
                     foreign key in a different table. If this is an unexpected constraint \
                     violation, please refer to our foreign key limitations in the documentation",
                    named(names, values.into_iter())
                )));
            }
        }
    }
    Ok(())
}
