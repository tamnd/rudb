//! Putting a nested column back together out of the leaves that store it.
//!
//! A nested column is stored as one column chunk per leaf, and each value in a leaf carries two
//! levels. The definition level says how far down the tree the value got before something was
//! missing, and the repetition level says which list the value starts a new element of. Every leaf
//! below a node agrees on the levels down to that node, so the walk here looks at the first leaf
//! below a node to decide what the node is, and then takes from every leaf below it.
//!
//! This decodes the whole row group of every leaf before it builds a row, and builds rows as
//! values, which is the simple way and not the fast one. A row group is what a writer cuts a file
//! into to keep it in memory, so it is the unit this holds.

use std::sync::Arc;

use rudb_common::{Error, LogicalType, Result, Value};
use rudb_compress::Codec;
use rudb_io::File;
use rudb_vector::Vector;

use crate::chunk::Pages;
use crate::metadata::{Nested, Node, Shape};
use crate::page::Body;

/// Where one leaf's column chunk is in the file.
#[derive(Debug, Clone)]
pub(crate) struct Part {
    pub start: u64,
    pub len: usize,
    pub codec: Codec,
    pub values: i64,
}

/// The levels and values of one leaf for a whole row group, and how far the walk has got.
struct Stream {
    reps: Vec<u32>,
    defs: Vec<u32>,
    values: Vec<Value>,
    at: usize,
}

/// The column `ty` of a row group of `rows` rows, out of the chunks of its leaves.
///
/// Returns the column and how many bytes were read for it.
///
/// # Errors
///
/// If a read or a decode fails, or the levels do not describe `rows` rows of the column's shape.
pub(crate) fn assemble(
    file: &dyn File,
    nested: &Nested,
    ty: &LogicalType,
    parts: &[Part],
    rows: usize,
) -> Result<(Vector, u64)> {
    let mut read = 0_u64;
    let mut streams = Vec::with_capacity(parts.len());
    for (leaf, part) in nested.leaves.iter().zip(parts) {
        let mut bytes = vec![0_u8; part.len];
        file.read_exact_at(part.start, &mut bytes)?;
        read = read.saturating_add(part.len as u64);
        let mut stream = Stream { reps: Vec::new(), defs: Vec::new(), values: Vec::new(), at: 0 };
        let mut dictionary: Option<Arc<Vector>> = None;
        for page in Pages::new(&bytes, part.codec, part.values) {
            let mut page = page?;
            match page.header.body {
                Body::Index => {}
                Body::Dictionary(_) => {
                    dictionary = Some(Arc::new(page.decode_dictionary(&leaf.column)?));
                }
                Body::DataV1(_) | Body::DataV2(_) => {
                    let (reps, defs, values) = page.decode_leaf(leaf, dictionary.as_ref())?;
                    stream.reps.extend(reps);
                    stream.defs.extend(defs);
                    stream.values.extend(values.iter());
                }
            }
        }
        streams.push(stream);
    }
    if streams.len() != nested.leaves.len() {
        return Err(Error::io("a nested column with a leaf that has no column chunk"));
    }
    let mut values = Vec::with_capacity(rows);
    for _ in 0..rows {
        values.push(value(&nested.root, ty, &mut streams)?);
    }
    if streams.iter().any(|stream| stream.at != stream.defs.len()) {
        return Err(Error::io(format!(
            "a nested column whose levels hold more than the {rows} rows of its row group"
        )));
    }
    Ok((Vector::from_values(ty.clone(), &values)?, read))
}

/// The next value of `node`, whose type is `ty`, taken from the leaves below it.
fn value(node: &Node, ty: &LogicalType, streams: &mut [Stream]) -> Result<Value> {
    let def = level(node, streams)?;
    if def < node.def {
        skip(node, streams);
        return Ok(Value::Null);
    }
    match (&node.shape, ty) {
        (Shape::Leaf(leaf), _) => {
            let stream = &mut streams[*leaf];
            let slot = stream.values.get_mut(stream.at).ok_or_else(|| {
                Error::io("a nested column with a leaf shorter than its levels".to_string())
            })?;
            stream.at += 1;
            Ok(std::mem::replace(slot, Value::Null))
        }
        (Shape::Struct(nodes), LogicalType::Struct(fields)) => {
            let mut members = Vec::with_capacity(nodes.len());
            for (node, field) in nodes.iter().zip(fields) {
                members.push((field.name.clone(), value(node, &field.ty, streams)?));
            }
            Ok(Value::Struct(members))
        }
        (Shape::List { def: full, rep, element }, LogicalType::List(element_ty)) => {
            let mut values = Vec::new();
            if def < *full {
                skip(node, streams);
            } else {
                loop {
                    values.push(value(element, element_ty, streams)?);
                    if !continues(node, *rep, streams) {
                        break;
                    }
                }
            }
            Ok(Value::List { element: (**element_ty).clone(), values })
        }
        (Shape::Map { def: full, rep, key, value: item }, LogicalType::Map(key_ty, value_ty)) => {
            let mut entries = Vec::new();
            if def < *full {
                skip(node, streams);
            } else {
                loop {
                    let k = value(key, key_ty, streams)?;
                    entries.push((k, value(item, value_ty, streams)?));
                    if !continues(node, *rep, streams) {
                        break;
                    }
                }
            }
            Ok(Value::Map { key: key_ty.clone(), value: value_ty.clone(), entries })
        }
        _ => Err(Error::internal(format!("a nested column whose shape is not its type {ty}"))),
    }
}

/// The definition level of the next entry of the first leaf below `node`.
fn level(node: &Node, streams: &[Stream]) -> Result<u32> {
    let stream = &streams[node.leaves.start];
    stream.defs.get(stream.at).copied().ok_or_else(|| {
        Error::io("a nested column whose levels ran out before its row group did".to_string())
    })
}

/// Whether the next entry below `node` is another element of the list it is, at level `rep`.
fn continues(node: &Node, rep: u32, streams: &[Stream]) -> bool {
    let stream = &streams[node.leaves.start];
    stream.reps.get(stream.at).is_some_and(|&next| next >= rep)
}

/// Steps every leaf below `node` past a node that is null or empty.
///
/// That is one entry, and every entry after it at a level above the node's, which is what DuckDB
/// writes for each element of a fixed size array that is null.
fn skip(node: &Node, streams: &mut [Stream]) {
    for stream in &mut streams[node.leaves.clone()] {
        stream.at += 1;
        while stream.reps.get(stream.at).is_some_and(|&next| next > node.rep) {
            stream.at += 1;
        }
    }
}
