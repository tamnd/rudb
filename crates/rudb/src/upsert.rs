//! What an `INSERT` with `ON CONFLICT`, `OR REPLACE` or `OR IGNORE` does with each row it brings.
//!
//! The pin decides every row on its own and in order. The first row of the statement with a given
//! key is the one that counts and a later row with the same key is dropped, whether or not the
//! table held the key already. A row whose key the table holds is a conflict with that held row,
//! and every other row is appended. A row with a null in the key takes part in no key, so it is
//! always appended.

use std::collections::{HashMap, HashSet};

use rudb_catalog::Key;
use rudb_common::{LogicalType, Result, Value};
use rudb_vector::{Chunk, VECTOR_SIZE, Vector};

/// What becomes of one row the statement brings.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Arrival {
    /// Appended.
    New,
    /// An earlier row of the statement had its key, or it clashes and the action is to do nothing.
    Dropped,
    /// It clashes with the held row at this place in the table.
    Held(usize),
}

/// The arrival of each new row, checked against the one key the conflict names or, with no key
/// named, against all of them.
pub(crate) fn arrivals(
    keys: &[Key],
    target: Option<usize>,
    held: &[Vec<Value>],
    new: &[Vec<Value>],
) -> Vec<Arrival> {
    let checked: Vec<&Key> = match target {
        Some(at) => vec![&keys[at]],
        None => keys.iter().collect(),
    };
    let mut places: Vec<HashMap<Box<[u8]>, usize>> = Vec::with_capacity(checked.len());
    for key in &checked {
        let mut place = HashMap::with_capacity(held.len());
        for (at, row) in held.iter().enumerate() {
            if let Some(encoded) = key.of_row(row) {
                place.insert(encoded, at);
            }
        }
        places.push(place);
    }
    let mut seen: Vec<HashSet<Box<[u8]>>> = vec![HashSet::new(); checked.len()];
    let mut out = Vec::with_capacity(new.len());
    for row in new {
        let encoded: Vec<Option<Box<[u8]>>> = checked.iter().map(|key| key.of_row(row)).collect();
        let repeated = encoded
            .iter()
            .zip(&seen)
            .any(|(encoded, seen)| encoded.as_ref().is_some_and(|key| seen.contains(key)));
        let arrival = if repeated {
            Arrival::Dropped
        } else {
            let hit = encoded.iter().zip(&places).find_map(|(encoded, place)| {
                encoded.as_ref().and_then(|key| place.get(key).copied())
            });
            match hit {
                Some(_) if target.is_none() => Arrival::Dropped,
                Some(at) => Arrival::Held(at),
                None => Arrival::New,
            }
        };
        for (encoded, seen) in encoded.into_iter().zip(&mut seen) {
            if let Some(encoded) = encoded {
                seen.insert(encoded);
            }
        }
        out.push(arrival);
    }
    out
}

/// Every row of these chunks as its values.
pub(crate) fn rows_of(chunks: &[Chunk]) -> Vec<Vec<Value>> {
    let mut rows = Vec::with_capacity(chunks.iter().map(Chunk::len).sum());
    for chunk in chunks {
        for row in 0..chunk.len() {
            rows.push(chunk.row(row).collect());
        }
    }
    rows
}

/// Rows of these types as chunks a table can hold.
pub(crate) fn chunks_of(types: &[LogicalType], rows: &[Vec<Value>]) -> Result<Vec<Chunk>> {
    let mut chunks = Vec::with_capacity(rows.len().div_ceil(VECTOR_SIZE));
    for part in rows.chunks(VECTOR_SIZE) {
        let mut columns = Vec::with_capacity(types.len());
        for (at, ty) in types.iter().enumerate() {
            let values: Vec<Value> = part.iter().map(|row| row[at].clone()).collect();
            columns.push(Vector::from_values(ty.clone(), &values)?);
        }
        chunks.push(Chunk::with_rows(columns, part.len())?);
    }
    Ok(chunks)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key(columns: &[usize]) -> Key {
        Key { columns: columns.to_vec(), primary: true }
    }

    fn rows(values: &[(i32, i32)]) -> Vec<Vec<Value>> {
        values.iter().map(|&(a, b)| vec![Value::Integer(a), Value::Integer(b)]).collect()
    }

    #[test]
    fn the_first_row_with_a_key_counts_and_a_later_one_is_dropped() {
        let held = rows(&[(1, 1), (2, 2)]);
        let new = rows(&[(1, 6), (1, 5), (3, 3), (3, 4), (2, 0)]);
        let got = arrivals(&[key(&[0])], Some(0), &held, &new);
        assert_eq!(
            got,
            [Arrival::Held(0), Arrival::Dropped, Arrival::New, Arrival::Dropped, Arrival::Held(1)]
        );
    }

    #[test]
    fn with_no_key_named_a_clash_on_any_key_drops_the_row() {
        let held = rows(&[(1, 1)]);
        let new = rows(&[(2, 1), (1, 2), (3, 3)]);
        let got = arrivals(&[key(&[0]), key(&[1])], None, &held, &new);
        assert_eq!(got, [Arrival::Dropped, Arrival::Dropped, Arrival::New]);
    }
}
