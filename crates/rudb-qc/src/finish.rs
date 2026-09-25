//! Turning what the compiled code left behind into chunks, and the stages between pipelines.
//!
//! Everything compiled code writes is a value at the width of its physical type, and a string is a
//! `str16`. [`vector`] builds a column out of those, one cell per row, which is the one place the
//! driver knows how a physical type is laid out. The group rows of a hash aggregate are read the
//! same way: each key is a cell with a null byte after it, and each accumulator is finished into a
//! cell by the rule its [`AccOp`] names.

use std::cmp::Ordering;

use rudb_catalog::{Catalog, QualifiedName};
use rudb_common::{Error, LogicalType, PhysicalType, Result, Value};
use rudb_kernels::compare::order;
use rudb_plan::{Node, NodeRef, Plan};
use rudb_qc_gen::{AccOp, Grouping, qir_type};
use rudb_qc_plan::{Column, Key, Kind};
use rudb_qc_rt::{Rt, text};
use rudb_vector::{Buffer, Chunk, Data, Selection, StringColumn, VECTOR_SIZE, Validity, Vector};

/// One value as compiled code wrote it, zero padded to sixteen bytes, or `None` for a null.
pub(crate) type Cell = Option<[u8; 16]>;

/// A column of type `ty` from its cells.
///
/// A string cell is a `str16`, and the bytes it points at are copied, so the vector owns them.
pub(crate) fn vector(ty: &LogicalType, cells: &[Cell]) -> Result<Vector> {
    macro_rules! fixed {
        ($variant:ident, $t:ty) => {{
            const W: usize = std::mem::size_of::<$t>();
            let v: Vec<$t> = cells
                .iter()
                .map(|c| {
                    c.map_or(<$t>::default(), |b| {
                        <$t>::from_le_bytes(b[..W].try_into().unwrap_or_default())
                    })
                })
                .collect();
            Data::$variant(Buffer::from(v))
        }};
    }
    let data = match ty.physical() {
        PhysicalType::Bool => Data::Bool(Buffer::from(
            cells.iter().map(|c| c.is_some_and(|b| b[0] != 0)).collect::<Vec<_>>(),
        )),
        PhysicalType::Int8 => fixed!(Int8, i8),
        PhysicalType::Int16 => fixed!(Int16, i16),
        PhysicalType::Int32 => fixed!(Int32, i32),
        PhysicalType::Int64 => fixed!(Int64, i64),
        PhysicalType::Int128 => fixed!(Int128, i128),
        PhysicalType::UInt8 => fixed!(UInt8, u8),
        PhysicalType::UInt16 => fixed!(UInt16, u16),
        PhysicalType::UInt32 => fixed!(UInt32, u32),
        PhysicalType::UInt64 => fixed!(UInt64, u64),
        PhysicalType::UInt128 => fixed!(UInt128, u128),
        PhysicalType::Float32 => fixed!(Float32, f32),
        PhysicalType::Float64 => fixed!(Float64, f64),
        PhysicalType::Varlen => {
            let mut s = StringColumn::with_capacity(cells.len());
            for c in cells {
                match c {
                    Some(b) => {
                        let header = u128::from_le_bytes(*b);
                        // SAFETY: every string compiled code writes is inline, points into a
                        // column the driver holds until the call returns, or points into the
                        // runtime's heap, which lives as long as the query. The caller reads the
                        // cells before either goes away.
                        s.push_bytes(unsafe { text::bytes(&header) });
                    }
                    None => {
                        s.push_bytes(&[]);
                    }
                }
            }
            Data::Varlen(s)
        }
        _ => return Err(Error::internal(format!("the driver has no column of type {ty}"))),
    };
    let valid: Vec<bool> = cells.iter().map(Option::is_some).collect();
    Ok(Vector::flat(ty.clone(), data)?.with_validity(Validity::from_run(&valid)))
}

/// A chunk of literal rows.
pub(crate) fn values(rows: &[Vec<Value>], columns: &[Column]) -> Result<Chunk> {
    let mut vectors = Vec::with_capacity(columns.len());
    for (i, c) in columns.iter().enumerate() {
        let column: Vec<Value> = rows.iter().map(|r| r[i].clone()).collect();
        vectors.push(Vector::from_values(c.ty.clone(), &column)?);
    }
    Chunk::with_rows(vectors, rows.len())
}

/// The groups of a hash aggregate: the keys and then the finished accumulators.
pub(crate) fn groups(rt: &Rt, g: &Grouping, columns: &[Column]) -> Result<Vec<Chunk>> {
    let table =
        rt.table(g.table).ok_or_else(|| Error::internal("the aggregate's table is gone"))?;
    let mut chunks = Vec::new();
    let mut from = 0;
    while from < table.len() {
        let to = (from + VECTOR_SIZE).min(table.len());
        let mut vectors = Vec::with_capacity(columns.len());
        for (k, ty) in &g.keys {
            let cells: Vec<Cell> = (from..to)
                .map(|gid| {
                    let row = table.row(gid);
                    let at = 8 + k.offset as usize;
                    (row[8 + k.null() as usize] == 0).then(|| cell(&row[at..at + k.width as usize]))
                })
                .collect();
            vectors.push(vector(ty, &cells)?);
        }
        for acc in &g.accs {
            let mut cells = Vec::with_capacity(to - from);
            for gid in from..to {
                let at = (g.acc_offset + acc.offset) as usize;
                let a = &table.row(gid)[at..];
                cells.push(finish(rt, acc.op, &acc.arg, a, gid)?);
            }
            vectors.push(vector(&acc.ty, &cells)?);
        }
        chunks.push(Chunk::with_rows(vectors, to - from)?);
        from = to;
    }
    Ok(chunks)
}

/// The value of one accumulator, `a` being its bytes in the group row.
fn finish(rt: &Rt, op: AccOp, arg: &LogicalType, a: &[u8], gid: usize) -> Result<Cell> {
    let i64_at = |at: usize| i64::from_le_bytes(a[at..at + 8].try_into().unwrap_or_default());
    let f64_at = |at: usize| f64::from_le_bytes(a[at..at + 8].try_into().unwrap_or_default());
    let i128_at = |at: usize| i128::from_le_bytes(a[at..at + 16].try_into().unwrap_or_default());
    Ok(match op {
        AccOp::CountStar | AccOp::Count => Some(cell(&a[..8])),
        AccOp::SumInt => (a[16] != 0).then(|| cell(&a[..16])),
        AccOp::SumFloat => (a[8] != 0).then(|| cell(&a[..8])),
        AccOp::AvgInt => {
            let n = i64_at(16);
            // The first engine's answer: the exact total, then one division in doubles.
            (n != 0).then(|| cell(&(i128_at(0) as f64 / n as f64).to_le_bytes()))
        }
        AccOp::AvgFloat => {
            let n = i64_at(8);
            (n != 0).then(|| cell(&(f64_at(0) / n as f64).to_le_bytes()))
        }
        AccOp::Min | AccOp::Max | AccOp::AnyValue => {
            let w = qir_type(arg).map_err(|r| Error::internal(r.to_string()))?.bytes() as usize;
            (a[w] != 0).then(|| cell(&a[..w]))
        }
        AccOp::MinStr | AccOp::MaxStr => (a[16] != 0).then(|| cell(&a[..16])),
        AccOp::Distinct(h) => {
            let d = rt.distinct(h).ok_or_else(|| Error::internal("a distinct set is gone"))?;
            let n = i64::try_from(d.count(gid)).unwrap_or(i64::MAX);
            Some(cell(&n.to_le_bytes()))
        }
    })
}

/// Up to sixteen bytes of a value, zero padded.
pub(crate) fn cell(b: &[u8]) -> [u8; 16] {
    let mut c = [0u8; 16];
    c[..b.len()].copy_from_slice(b);
    c
}

/// The rows of `chunks` in the order of `keys`, then `offset` of them skipped and `count` kept.
///
/// Only the key columns are copied out, and what is sorted is row positions, so a top ten over ten
/// million groups (ClickBench q33) holds one value per group and not a whole row. With a `count`
/// the positions past it are cut with a selection before the sort. Equal keys keep the order they
/// came in, which is what the stable sort this replaced gave, so the answer did not change.
pub(crate) fn sort(
    chunks: Vec<Chunk>,
    keys: &[Key],
    count: Option<u64>,
    offset: u64,
) -> Result<Vec<Chunk>> {
    let Some(types) = chunks.first().map(Chunk::types) else {
        return Ok(Vec::new());
    };
    let keys: Vec<(usize, bool, bool)> = keys
        .iter()
        .map(|k| match k.expr.kind {
            Kind::Column(c) => Ok((c, k.descending, k.nulls_first)),
            _ => Err(Error::internal("a sort key the check let through")),
        })
        .collect::<Result<_>>()?;
    let mut place: Vec<(u32, u32)> = Vec::new();
    let mut columns: Vec<Vec<Value>> = vec![Vec::new(); keys.len()];
    for (at, chunk) in chunks.iter().enumerate() {
        for (column, &(c, _, _)) in columns.iter_mut().zip(&keys) {
            let vector = chunk.column(c)?;
            column.extend((0..chunk.len()).map(|i| vector.value_at(i)));
        }
        place.extend((0..chunk.len()).map(|i| (at as u32, i as u32)));
    }
    let mut failed = None;
    let mut compare = |l: &u32, r: &u32| {
        let (l, r) = (*l as usize, *r as usize);
        for (column, &(_, descending, nulls_first)) in columns.iter().zip(&keys) {
            let (a, b) = (&column[l], &column[r]);
            let o = match (a.is_null(), b.is_null()) {
                (true, true) => Ordering::Equal,
                (true, false) => {
                    if nulls_first {
                        Ordering::Less
                    } else {
                        Ordering::Greater
                    }
                }
                (false, true) => {
                    if nulls_first {
                        Ordering::Greater
                    } else {
                        Ordering::Less
                    }
                }
                (false, false) => match order(a, b) {
                    Ok(o) if descending => o.reverse(),
                    Ok(o) => o,
                    Err(e) => {
                        failed.get_or_insert(e);
                        Ordering::Equal
                    }
                },
            };
            if o != Ordering::Equal {
                return o;
            }
        }
        l.cmp(&r)
    };
    let skip = usize::try_from(offset).unwrap_or(usize::MAX);
    let take = count.map_or(usize::MAX, |n| usize::try_from(n).unwrap_or(usize::MAX));
    let wanted = skip.saturating_add(take);
    let mut kept: Vec<u32> = (0..place.len() as u32).collect();
    if wanted == 0 {
        kept.clear();
    } else if wanted < kept.len() {
        kept.select_nth_unstable_by(wanted - 1, &mut compare);
        kept.truncate(wanted);
    }
    kept.sort_unstable_by(&mut compare);
    if let Some(e) = failed {
        return Err(e);
    }
    let rows: Vec<Vec<Value>> = kept
        .iter()
        .skip(skip)
        .map(|&n| {
            let (at, i) = place[n as usize];
            chunks[at as usize].row(i as usize).collect()
        })
        .collect();
    let mut out = Vec::new();
    for part in rows.chunks(VECTOR_SIZE) {
        let mut vectors = Vec::with_capacity(types.len());
        for (c, ty) in types.iter().enumerate() {
            let column: Vec<Value> = part.iter().map(|r| r[c].clone()).collect();
            vectors.push(Vector::from_values(ty.clone(), &column)?);
        }
        out.push(Chunk::with_rows(vectors, part.len())?);
    }
    Ok(out)
}

/// The table rows named by column `ordinal` of `chunks`, read back in the order they are named.
///
/// This is `rudb_exec`'s `TableFetch` over rows the compiled engine produced: the ordinals are read
/// in file order, each once, and then put back into the order the stage before gave them.
pub(crate) fn fetch(
    chunks: Vec<Chunk>,
    plan: &Plan,
    node: NodeRef,
    ordinal: usize,
    catalog: &Catalog,
) -> Result<Vec<Chunk>> {
    let Node::TableFetch { catalog: c, schema, table, columns, .. } = *plan.node(node) else {
        return Err(Error::internal("a fetch stage that names no TableFetch"));
    };
    let name = QualifiedName::new(plan.string(c), plan.string(schema), plan.string(table));
    let table = catalog.table(&name)?;
    let fields = plan.field_list(columns);
    let positions = fields
        .iter()
        .map(|f| {
            table
                .column_index(&f.name)
                .ok_or_else(|| Error::internal(format!("{name} has no column {}", f.name)))
        })
        .collect::<Result<Vec<_>>>()?;
    let types: Vec<LogicalType> = fields.iter().map(|f| f.ty.clone()).collect();
    let mut out = Vec::with_capacity(chunks.len());
    for chunk in chunks {
        let count = chunk.len();
        if count == 0 {
            continue;
        }
        let mut held = Vec::with_capacity(count);
        for i in 0..count {
            match chunk.column(ordinal)?.value_at(i) {
                Value::BigInt(n) => held
                    .push(u64::try_from(n).map_err(|_| Error::internal("a negative row ordinal"))?),
                other => return Err(Error::internal(format!("a row ordinal of {other:?}"))),
            }
        }
        let mut order: Vec<usize> = (0..count).collect();
        order.sort_by_key(|&at| held[at]);
        let mut rows = Vec::with_capacity(count);
        let mut taken = vec![0_u32; count];
        for at in order {
            if rows.last() != Some(&held[at]) {
                rows.push(held[at]);
            }
            taken[at] = u32::try_from(rows.len() - 1).unwrap_or(u32::MAX);
        }
        let fetched = table.rows().rows_at(&types, &positions, &rows)?;
        let mut vectors = Vec::with_capacity(fetched.width());
        for at in 0..fetched.width() {
            vectors.push(fetched.column(at)?.gather(&taken)?);
        }
        out.push(Chunk::with_rows(vectors, count)?);
    }
    Ok(out)
}

/// The rows of `chunks` in the order they came, `offset` of them skipped and `count` kept.
pub(crate) fn limit(chunks: Vec<Chunk>, count: Option<u64>, offset: u64) -> Result<Vec<Chunk>> {
    let mut skip = usize::try_from(offset).unwrap_or(usize::MAX);
    let mut left = count.map_or(usize::MAX, |n| usize::try_from(n).unwrap_or(usize::MAX));
    let mut out = Vec::new();
    for chunk in chunks {
        if left == 0 {
            break;
        }
        let n = chunk.len();
        if skip >= n {
            skip -= n;
            continue;
        }
        let end = n.min(skip.saturating_add(left));
        let kept = end - skip;
        let chunk = if skip == 0 && end == n {
            chunk
        } else {
            let at: Vec<u32> = (skip..end).map(|i| i as u32).collect();
            chunk.select(&Selection::from_indices(at))?
        };
        out.push(chunk);
        left -= kept;
        skip = 0;
    }
    Ok(out)
}
