//! The kernel behind a `vcall`: a scalar function the generator has no translator for, run by the
//! first engine's own code, per section 7.10 of `spec/compiler/07-code-generation.md`.
//!
//! The first engine evaluates a bound function call by building a [`Recipe`] from the name and
//! the arguments that are literals, once, and then handing it the argument vectors with
//! [`rudb_kernels::call_prepared`] for every chunk. A [`Call`] does exactly that, over vectors of
//! one row made from the slots the generated code stored the arguments in, so its answers and its
//! errors are the first engine's by construction rather than by a second implementation that has
//! to be kept in step.
//!
//! The call is one row at a time for now. The buffers a `vcall` passes are, per argument, the
//! address of its value and the address of its validity byte, and then the same two for the
//! answer. A value is at the width of its physical type and a string is a `str16`. Batching up to
//! 1,024 rows behind a stage boundary is what the spec asks for in the end, and it changes where
//! the buffers are and how many rows they hold, not what the kernel computes.

#![allow(unsafe_code)]

use std::fmt::{self, Write};

use rudb_common::{Error, LogicalType, PhysicalType, Result, Value};
use rudb_kernels::Recipe;
use rudb_qc_plan::{Column, Expr, Kind};
use rudb_qc_rt::text::{self, Heap};
use rudb_vector::{Buffer, Data, StringColumn, Validity, Vector};

/// One call site's function, with the per query work done.
pub(crate) struct Call {
    recipe: Recipe,
    args: Vec<LogicalType>,
    returns: LogicalType,
    /// How the call is written, for the division by zero message that quotes it.
    written: String,
    /// The long strings the function answered with. They are kept for as long as the kernel is,
    /// which is as long as the runtime and so the query, because the result sink and the group
    /// table read them after the body has moved on to the next row.
    heap: Heap,
}

impl fmt::Debug for Call {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Call").field("function", &self.recipe.name()).finish_non_exhaustive()
    }
}

impl Call {
    /// The call of `name` over `args`, answering `returns`, with the columns it reads named by
    /// `columns` for the message that quotes it.
    pub(crate) fn new(
        name: &str,
        args: &[Expr],
        returns: &LogicalType,
        columns: &[Column],
    ) -> Call {
        let literals: Vec<Option<Value>> = args
            .iter()
            .map(|a| match &a.kind {
                Kind::Constant(v) => Some(v.clone()),
                _ => None,
            })
            .collect();
        let mut written = String::new();
        // A `String` never fails to take a write, so there is nothing to report.
        let _ = call(&mut written, name, args, columns);
        Call {
            recipe: Recipe::new(name, &literals),
            args: args.iter().map(|a| a.ty.clone()).collect(),
            returns: returns.clone(),
            written,
            heap: Heap::new(),
        }
    }

    /// Runs the function over the `n` rows in `buffers`.
    pub(crate) fn run(&mut self, n: u64, buffers: &[u128]) -> Result<()> {
        if n != 1 || buffers.len() != 2 * self.args.len() + 2 {
            return Err(Error::internal(format!(
                "a vcall of {} passed {n} rows and {} buffers",
                self.recipe.name(),
                buffers.len()
            )));
        }
        let mut args = Vec::with_capacity(self.args.len());
        for (k, ty) in self.args.iter().enumerate() {
            // SAFETY: the generated code passes the addresses of a sixteen byte value slot and a
            // validity byte in its own state, which lives for the whole call.
            let (value, valid) = unsafe { (read(buffers[2 * k]), *addr(buffers[2 * k + 1])) };
            args.push(one(ty, (valid != 0).then_some(value))?);
        }
        let written = || self.written.clone();
        let answer =
            rudb_kernels::call_prepared(&self.recipe, &args, &self.returns, Some(&written))?;
        let (bytes, valid) = self.cell(answer)?;
        let at = self.args.len() * 2;
        // SAFETY: the answer's slots are sixteen bytes and one byte of the same state.
        unsafe {
            std::ptr::write_unaligned(addr(buffers[at]).cast::<[u8; 16]>(), bytes);
            *addr(buffers[at + 1]) = u8::from(valid);
        }
        Ok(())
    }

    /// The first row of `answer` as compiled code reads it, zero when it is null so that the value
    /// under an invalid answer is as harmless as every other one.
    fn cell(&mut self, answer: Vector) -> Result<([u8; 16], bool)> {
        let flat = answer.into_flat()?;
        if flat.is_empty() || flat.is_null_at(0) {
            return Ok(([0; 16], false));
        }
        let mut out = [0u8; 16];
        let mut put = |b: &[u8]| out[..b.len()].copy_from_slice(b);
        let data = flat.data().ok_or_else(|| Error::internal("a flattened answer is not flat"))?;
        let physical = match data {
            Data::Bool(b) => {
                put(&[u8::from(b.as_slice()[0])]);
                PhysicalType::Bool
            }
            Data::Int8(b) => {
                put(&b.as_slice()[0].to_le_bytes());
                PhysicalType::Int8
            }
            Data::Int16(b) => {
                put(&b.as_slice()[0].to_le_bytes());
                PhysicalType::Int16
            }
            Data::Int32(b) => {
                put(&b.as_slice()[0].to_le_bytes());
                PhysicalType::Int32
            }
            Data::Int64(b) => {
                put(&b.as_slice()[0].to_le_bytes());
                PhysicalType::Int64
            }
            Data::Int128(b) => {
                put(&b.as_slice()[0].to_le_bytes());
                PhysicalType::Int128
            }
            Data::UInt8(b) => {
                put(&b.as_slice()[0].to_le_bytes());
                PhysicalType::UInt8
            }
            Data::UInt16(b) => {
                put(&b.as_slice()[0].to_le_bytes());
                PhysicalType::UInt16
            }
            Data::UInt32(b) => {
                put(&b.as_slice()[0].to_le_bytes());
                PhysicalType::UInt32
            }
            Data::UInt64(b) => {
                put(&b.as_slice()[0].to_le_bytes());
                PhysicalType::UInt64
            }
            Data::UInt128(b) => {
                put(&b.as_slice()[0].to_le_bytes());
                PhysicalType::UInt128
            }
            Data::Float32(b) => {
                put(&b.as_slice()[0].to_le_bytes());
                PhysicalType::Float32
            }
            Data::Float64(b) => {
                put(&b.as_slice()[0].to_le_bytes());
                PhysicalType::Float64
            }
            Data::Varlen(s) => {
                let bytes = s.views()[0].bytes_in(s.arena()).unwrap_or_default();
                put(&self.heap.keep(bytes).to_le_bytes());
                PhysicalType::Varlen
            }
            _ => PhysicalType::Empty,
        };
        // The generated code reads the answer at the width of the type the binder resolved, so an
        // answer of another width would be read wrong rather than refused.
        if physical != self.returns.physical() {
            return Err(Error::internal(format!(
                "{} answered a {physical:?} value for a {} call",
                self.recipe.name(),
                self.returns
            )));
        }
        Ok((out, true))
    }
}

fn addr(at: u128) -> *mut u8 {
    std::ptr::with_exposed_provenance_mut(at as u64 as usize)
}

/// The sixteen bytes at `at`.
///
/// # Safety
///
/// `at` is the address of sixteen readable bytes.
unsafe fn read(at: u128) -> [u8; 16] {
    // SAFETY: the caller's contract.
    unsafe { std::ptr::read_unaligned(addr(at).cast::<[u8; 16]>()) }
}

/// A vector of one row of type `ty`, holding the value compiled code wrote as `cell`, or a null.
fn one(ty: &LogicalType, cell: Option<[u8; 16]>) -> Result<Vector> {
    let b = cell.unwrap_or([0; 16]);
    macro_rules! fixed {
        ($variant:ident, $t:ty) => {{
            const W: usize = std::mem::size_of::<$t>();
            let mut x = [0u8; W];
            x.copy_from_slice(&b[..W]);
            Data::$variant(Buffer::from(vec![<$t>::from_le_bytes(x)]))
        }};
    }
    let data = match ty.physical() {
        PhysicalType::Bool => Data::Bool(Buffer::from(vec![b[0] != 0])),
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
            let mut s = StringColumn::with_capacity(1);
            let header = u128::from_le_bytes(b);
            // SAFETY: a string compiled code holds is inline, points into a column the driver
            // keeps alive for the call, or points into a heap the runtime keeps for the query.
            // A null's header is all zeros, which is the empty string.
            s.push_bytes(unsafe { text::bytes(&header) });
            Data::Varlen(s)
        }
        _ => return Err(Error::internal(format!("a vcall argument of type {ty}"))),
    };
    let validity = if cell.is_some() { Validity::AllValid } else { Validity::AllInvalid };
    Ok(Vector::flat(ty.clone(), data)?.with_validity(validity))
}

/// Writes `e` the way the first engine's error messages quote a bound expression, which is what
/// `rudb-exec` does in `written.rs` over the plan. A column is the name its source gives it.
fn form<W: Write>(out: &mut W, e: &Expr, columns: &[Column]) -> fmt::Result {
    match &e.kind {
        Kind::Column(i) => match columns.get(*i) {
            Some(c) => out.write_str(&c.name),
            None => write!(out, "#{i}"),
        },
        Kind::Constant(v @ Value::Interval { .. }) => write!(out, "'{v}'::INTERVAL"),
        Kind::Constant(v) => write!(out, "{v}"),
        Kind::Cast { input, try_cast } => {
            out.write_str(if *try_cast { "TRY_CAST(" } else { "CAST(" })?;
            form(out, input, columns)?;
            write!(out, " AS {})", e.ty)
        }
        Kind::Compare { op, left, right } => {
            out.write_char('(')?;
            form(out, left, columns)?;
            write!(out, " {} ", op.symbol())?;
            form(out, right, columns)?;
            out.write_char(')')
        }
        Kind::And(children) | Kind::Or(children) => {
            let keyword = if matches!(e.kind, Kind::And(_)) { "AND" } else { "OR" };
            out.write_char('(')?;
            for (k, c) in children.iter().enumerate() {
                if k > 0 {
                    write!(out, " {keyword} ")?;
                }
                form(out, c, columns)?;
            }
            out.write_char(')')
        }
        Kind::Function { name, args } => call(out, name, args, columns),
        Kind::Case { arms, otherwise } => {
            out.write_str("CASE")?;
            for (when, then) in arms {
                out.write_str(" WHEN ")?;
                form(out, when, columns)?;
                out.write_str(" THEN ")?;
                form(out, then, columns)?;
            }
            if let Some(o) = otherwise {
                out.write_str(" ELSE ")?;
                form(out, o, columns)?;
            }
            out.write_str(" END")
        }
    }
}

/// A binary operator between its operands and anything else in front of them, with the internal
/// names spelled the way the first engine spells them.
fn call<W: Write>(out: &mut W, name: &str, args: &[Expr], columns: &[Column]) -> fmt::Result {
    let name = match name {
        "__rudb_checked_slash" => "/",
        "__rudb_checked_remainder" => "%",
        "__rudb_divide" => "divide",
        "__rudb_mod" => "mod",
        other => other,
    };
    let operator = !name.starts_with(|first: char| first.is_alphabetic() || first == '_');
    match (operator, args) {
        (true, [left, right]) => {
            out.write_char('(')?;
            form(out, left, columns)?;
            write!(out, " {name} ")?;
            form(out, right, columns)?;
            out.write_char(')')
        }
        _ => {
            write!(out, "{name}(")?;
            for (k, a) in args.iter().enumerate() {
                if k > 0 {
                    out.write_str(", ")?;
                }
                form(out, a, columns)?;
            }
            out.write_char(')')
        }
    }
}
