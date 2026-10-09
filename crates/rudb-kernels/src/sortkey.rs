//! `create_sort_key(key, modifier, ...)`, the bytes the pin sorts rows by, written byte for byte
//! the way it writes them.
//!
//! Every value starts with a validity byte, which is 1 or 2 by where the key puts its nulls and is
//! never flipped. What follows is the value's bytes, flipped for a descending key. A string adds
//! one to every byte and ends in a zero, a blob escapes a zero or a one with a one and ends in a
//! zero, and a list or an array ends in a zero, flipped like the rest. A struct is its fields one
//! after another. Inside a nested value nulls go last for an ascending key and first for a
//! descending one whatever the call asked for at the top, which the pin does after PostgreSQL.
//!
//! The binder reads the modifiers once and writes each of them back as `ASC NULLS LAST` or one of
//! the other three, and passes the types of the keys last as an empty list of a struct of them.
//! A null carries no type, and a null struct still writes each of its fields as a null, and a null
//! array each of its elements and its end, so the bytes cannot be had from the values alone.

use rudb_common::{Error, LogicalType, PhysicalType, Result, Value};

use crate::topk::payload;

/// Where a key puts its nulls and whether its bytes are flipped.
#[derive(Clone, Copy)]
struct Order {
    descending: bool,
    null: u8,
    valid: u8,
}

impl Order {
    fn new(descending: bool, nulls_first: bool) -> Self {
        let (null, valid) = if nulls_first { (1, 2) } else { (2, 1) };
        Self { descending, null, valid }
    }

    /// The order of a value inside a nested one.
    fn inner(self) -> Self {
        Self::new(self.descending, self.descending)
    }

    /// The byte a list or an array ends in.
    fn end(self) -> u8 {
        if self.descending { 0xff } else { 0 }
    }
}

/// The answer of `create_sort_key` or `index_key` when `name` is one of them, or `None` for any
/// other function.
pub(crate) fn value(name: &str, args: &[Value], returns: &LogicalType) -> Option<Result<Value>> {
    match name {
        "create_sort_key" => Some(create(args, returns)),
        "index_key" => Some(index_key(args)),
        _ => None,
    }
}

/// `index_key(path, name, key, ...)`, the key the pin's ART index holds a row under.
///
/// The binder has found the index, cast each key to the type of its column and put the types last
/// the way it does for a sort key, and passes the keys alone. A key is the bytes of each value one
/// after another the way a sort key writes them, with no validity byte and nothing flipped, and a
/// string is escaped and ended like a blob. A null anywhere makes the whole key null.
fn index_key(args: &[Value]) -> Result<Value> {
    let Some((Value::List { element: LogicalType::Struct(types), .. }, keys)) = args.split_last()
    else {
        return Err(Error::internal("index_key takes the types of its keys last"));
    };
    let mut out = Vec::new();
    for (key, field) in keys.iter().zip(types) {
        match (&field.ty, key) {
            _ if key.is_null() => return Ok(Value::Null),
            (LogicalType::Enum(_), _) => scalar(key, &field.ty, &mut out),
            (_, Value::Varchar(text)) => payload(&Value::Blob(text.as_bytes().to_vec()), &mut out),
            _ => scalar(key, &field.ty, &mut out),
        }
    }
    Ok(Value::Blob(out))
}

fn create(args: &[Value], returns: &LogicalType) -> Result<Value> {
    let Some((Value::List { element: LogicalType::Struct(types), .. }, pairs)) = args.split_last()
    else {
        return Err(Error::internal("create_sort_key takes the types of its keys last"));
    };
    let mut out = Vec::new();
    for (pair, field) in pairs.chunks_exact(2).zip(types) {
        let Value::Varchar(modifier) = &pair[1] else {
            return Err(Error::internal("create_sort_key takes its modifiers as text"));
        };
        let order = Order::new(modifier.starts_with("DESC"), modifier.ends_with("FIRST"));
        encode(&pair[0], &field.ty, order, &mut out);
    }
    if *returns == LogicalType::BigInt {
        let mut word = [0; 8];
        let Some(head) = word.get_mut(..out.len()) else {
            return Err(Error::internal("create_sort_key has more than eight bytes for a BIGINT"));
        };
        head.copy_from_slice(&out);
        return Ok(Value::BigInt(i64::from_be_bytes(word)));
    }
    Ok(Value::Blob(out))
}

fn encode(value: &Value, ty: &LogicalType, order: Order, out: &mut Vec<u8>) {
    let inner = order.inner();
    if value.is_null() {
        out.push(order.null);
        match ty {
            LogicalType::Struct(fields) => {
                for field in fields {
                    encode(&Value::Null, &field.ty, inner, out);
                }
            }
            LogicalType::Union(members) => {
                encode(&Value::Null, &LogicalType::UTinyInt, inner, out);
                for member in members {
                    encode(&Value::Null, &member.ty, inner, out);
                }
            }
            LogicalType::Array(element, size) => {
                for _ in 0..*size {
                    encode(&Value::Null, element, inner, out);
                }
                out.push(order.end());
            }
            _ => {}
        }
        return;
    }
    out.push(order.valid);
    match (ty, value) {
        (LogicalType::Struct(fields), Value::Struct(values)) => {
            for (field, (_, value)) in fields.iter().zip(values) {
                encode(value, &field.ty, inner, out);
            }
        }
        // A union is a struct of its tag and every member, with the members it does not hold null.
        (LogicalType::Union(members), Value::Union { tag, value, .. }) => {
            encode(&Value::UTinyInt(*tag), &LogicalType::UTinyInt, inner, out);
            for (at, member) in members.iter().enumerate() {
                let held = if at == usize::from(*tag) { &**value } else { &Value::Null };
                encode(held, &member.ty, inner, out);
            }
        }
        (
            LogicalType::List(element) | LogicalType::Array(element, _),
            Value::List { values, .. },
        ) => {
            for value in values {
                encode(value, element, inner, out);
            }
            out.push(order.end());
        }
        // A map is a list of two field structs, and an entry is never null.
        (LogicalType::Map(key, held), Value::Map { entries, .. }) => {
            for (one, value) in entries {
                out.push(inner.valid);
                encode(one, key, inner, out);
                encode(value, held, inner, out);
            }
            out.push(order.end());
        }
        _ => {
            let start = out.len();
            scalar(value, ty, out);
            if order.descending {
                for byte in &mut out[start..] {
                    *byte = !*byte;
                }
            }
        }
    }
}

/// The bytes of a value that is not nested and not null.
fn scalar(value: &Value, ty: &LogicalType, out: &mut Vec<u8>) {
    match (ty, value) {
        (LogicalType::Enum(labels), Value::Varchar(label)) => {
            let code = labels.iter().position(|one| one == label).unwrap_or_default();
            let width = ty.physical().size();
            out.extend_from_slice(&code.to_be_bytes()[size_of::<usize>() - width..]);
        }
        // The key a zoned time is held in is not the pin's bits, and a sort key is.
        (_, Value::TimeTz(key)) => {
            payload(&Value::BigInt(rudb_common::time_tz::bits(*key)), out);
        }
        _ => payload(value, out),
    }
}

/// How many bytes a key of this type always takes, its validity byte included, or `None` when
/// that depends on the value. The pin answers with a BIGINT when every key has one and they come
/// to no more than eight.
#[must_use]
pub fn fixed_width(ty: &LogicalType) -> Option<usize> {
    match ty.physical() {
        PhysicalType::Varlen | PhysicalType::List | PhysicalType::Array | PhysicalType::Struct => {
            None
        }
        // The pin stores a null of no type as an INTEGER.
        PhysicalType::Empty => Some(5),
        physical => Some(physical.size() + 1),
    }
}
