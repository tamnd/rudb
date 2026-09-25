//! Handing chunks to a compiled body, one chunk to a morsel.
//!
//! The body reads its columns through the morsel's column table, which wants a values address and
//! a validity bitmap per column. A fixed width column of a flat vector is already the first of
//! those and is passed as it is. A string column is not, because a `StringView` holds an offset
//! into its column's arena and compiled code wants an address it can read without knowing which
//! column the string came from, so each view is rewritten as a `str16` into a buffer that lives as
//! long as the call.
//!
//! A result body writes its rows into buffers the driver points it at, and they are turned into a
//! chunk straight after the call. The strings in them are copied out then too, because they point
//! into the chunk that was just read or into the runtime's heap, and neither is kept.

use std::fmt;
use std::sync::{Arc, Mutex, MutexGuard};

use rudb_common::{Cancel, Error, ErrorCode, Result};
use rudb_pipeline::{Lease, Progress, Sink};
use rudb_plan::Plan;
use rudb_qc_gen::{Body, Out};
use rudb_qc_interp::Program;
use rudb_qc_ir::{ErrorKind, Module, status};
use rudb_qc_rt::abi::{Col, Morsel};
use rudb_qc_rt::{RUNTIME_ERROR, Rt, text};
use rudb_vector::{Chunk, Data, Validity, Vector};

use crate::Under;
use crate::finish::{Cell, cell, vector};

/// One pipeline being run.
pub(crate) struct Feed<'a> {
    module: &'a Module,
    program: &'a Program,
    func: usize,
    body: &'a Body,
    cancel: Cancel,
    inner: Mutex<Inner<'a>>,
}

/// What a call changes.
struct Inner<'a> {
    rt: &'a mut Rt,
    /// The body's state, in words so that every field in it is aligned.
    state: Vec<u64>,
    /// The chunks a result body produced.
    out: Vec<Chunk>,
}

impl fmt::Debug for Feed<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Feed").field("func", &self.body.func).finish_non_exhaustive()
    }
}

impl<'a> Feed<'a> {
    /// A feed for `body`, with its state set up.
    pub(crate) fn new(
        module: &'a Module,
        program: &'a Program,
        body: &'a Body,
        rt: &'a mut Rt,
        cancel: &Cancel,
    ) -> Result<Feed<'a>> {
        let func = program
            .func(&body.func)
            .ok_or_else(|| Error::internal(format!("no function {} in the module", body.func)))?;
        let mut state = vec![0u64; (body.state as usize).div_ceil(8)];
        if let Out::Aggregate(g) = &body.sink {
            if let Some(at) = g.row {
                // The one group of an aggregate with no groups was made with the table, and rows
                // never move, so its address is written once.
                let table = rt.table(g.table).ok_or_else(|| {
                    Error::internal("the aggregate's table is not in the runtime")
                })?;
                let row = table.address(0) as u64;
                bytes(&mut state)[at as usize..at as usize + 8].copy_from_slice(&row.to_le_bytes());
            }
        }
        let cancel = cancel.clone();
        Ok(Feed {
            module,
            program,
            func,
            body,
            cancel,
            inner: Mutex::new(Inner { rt, state, out: Vec::new() }),
        })
    }

    /// Runs the body over every chunk of a scan, by building `scan` in the first engine with this
    /// feed as the root.
    pub(crate) fn scan(&self, scan: &Plan, under: Under<'_>) -> Result<()> {
        let sink = Arc::new(Scan(self));
        let query = rudb_exec::build_measured_into(
            scan,
            under.catalog,
            under.cancel,
            under.memory,
            under.seams,
            under.session,
            sink,
        )?;
        query.run(under.cancel, under.pool)
    }

    /// Runs the body over one chunk.
    pub(crate) fn push(&self, chunk: &Chunk) -> Result<()> {
        let chunk = chunk.clone().settled()?.into_flat()?;
        let rows = chunk.len();
        if rows == 0 {
            return Ok(());
        }
        let mut held = Vec::with_capacity(self.body.reads.len());
        for &c in &self.body.reads {
            held.push(Held::of(chunk.column(c)?, rows)?);
        }
        let cols: Vec<Col> = held.iter().map(Held::col).collect();
        let morsel = Morsel {
            source: 0,
            chunk: 0,
            begin: 0,
            end: u32::try_from(rows)
                .map_err(|_| Error::internal("a chunk too long for a morsel"))?,
            seq: 0,
            enc: 0,
            flags: 0,
            cols: cols.as_ptr(),
        };
        let mut inner = self.lock();
        let mut buffers = Vec::new();
        if let Out::Result { count, columns } = &self.body.sink {
            let st = bytes(&mut inner.state);
            st[*count as usize..*count as usize + 8].fill(0);
            for slot in columns {
                let mut b = Buffers {
                    values: vec![0u128; (rows * slot.ty.bytes() as usize).div_ceil(16)],
                    valid: vec![0u8; rows],
                };
                let values = b.values.as_mut_ptr() as u64;
                let valid = b.valid.as_mut_ptr() as u64;
                st[slot.values as usize..slot.values as usize + 8]
                    .copy_from_slice(&values.to_le_bytes());
                st[slot.valid as usize..slot.valid as usize + 8]
                    .copy_from_slice(&valid.to_le_bytes());
                buffers.push(b);
            }
        }
        let Inner { rt, state, out } = &mut *inner;
        let st = state.as_mut_ptr().cast::<u8>();
        let status = self.program.call(self.func, st, (&raw const morsel).cast(), &mut **rt);
        drop(held);
        self.check(status, rt)?;
        if let Out::Result { count, columns } = &self.body.sink {
            let st = bytes(state);
            let n = u64::from_le_bytes(
                st[*count as usize..*count as usize + 8].try_into().unwrap_or_default(),
            );
            let n = usize::try_from(n).unwrap_or(usize::MAX).min(rows);
            let mut vectors = Vec::with_capacity(columns.len());
            for (slot, b) in columns.iter().zip(&buffers) {
                let w = slot.ty.bytes() as usize;
                // SAFETY: the buffer is `rows * w` bytes long and was allocated as `u128`s, which
                // any byte pattern is.
                let values = unsafe {
                    std::slice::from_raw_parts(b.values.as_ptr().cast::<u8>(), b.values.len() * 16)
                };
                let cells: Vec<Cell> = (0..n)
                    .map(|i| (b.valid[i] != 0).then(|| cell(&values[i * w..(i + 1) * w])))
                    .collect();
                vectors.push(vector(&slot.logical, &cells)?);
            }
            out.push(Chunk::with_rows(vectors, n)?);
        }
        Ok(())
    }

    /// The chunks a result body produced, once every chunk has been pushed.
    pub(crate) fn finish(self) -> Result<Vec<Chunk>> {
        let inner = self.inner.into_inner().map_err(|_| Error::internal("a feed was poisoned"))?;
        Ok(inner.out)
    }

    fn lock(&self) -> MutexGuard<'_, Inner<'a>> {
        self.inner.lock().unwrap_or_else(|held| held.into_inner())
    }

    /// Turns a status other than `OK` into the error it stands for.
    fn check(&self, s: u64, rt: &mut Rt) -> Result<()> {
        match status::kind(s) {
            status::OK => Ok(()),
            status::CANCELLED => {
                self.cancel.check()?;
                Err(Error::interrupt("Interrupted!"))
            }
            status::ERROR if status::payload(s) == RUNTIME_ERROR => Err(rt
                .take_error()
                .unwrap_or_else(|| Error::internal("the runtime failed and did not say why"))),
            status::ERROR => {
                let site = usize::try_from(status::payload(s))
                    .ok()
                    .and_then(|at| self.module.errors.get(at))
                    .ok_or_else(|| Error::internal(format!("status {s:#x} names no error site")))?;
                let code = match site.kind {
                    ErrorKind::Overflow | ErrorKind::DivideByZero | ErrorKind::OutOfRange => {
                        ErrorCode::OutOfRange
                    }
                    ErrorKind::Conversion => ErrorCode::Conversion,
                    ErrorKind::Cancel => ErrorCode::Interrupt,
                    ErrorKind::Internal => return Err(Error::internal(site.text.clone())),
                };
                Err(Error::new(code, site.text.clone()))
            }
            other => Err(Error::internal(format!("a pipeline returned status {other}"))),
        }
    }
}

/// The output buffers of one result column for one call.
struct Buffers {
    values: Vec<u128>,
    valid: Vec<u8>,
}

/// The state as bytes.
fn bytes(state: &mut [u64]) -> &mut [u8] {
    // SAFETY: a `u64` is eight bytes with no padding and any byte pattern is one.
    unsafe { std::slice::from_raw_parts_mut(state.as_mut_ptr().cast::<u8>(), state.len() * 8) }
}

/// A source column as the body reads it, and whatever had to be made for that.
struct Held<'c> {
    values: *const u8,
    valid: Vec<u8>,
    /// The `str16` headers of a string column, which `values` points at.
    _text: Vec<u128>,
    _chunk: std::marker::PhantomData<&'c Vector>,
}

impl<'c> Held<'c> {
    fn of(v: &'c Vector, rows: usize) -> Result<Held<'c>> {
        let mut valid = vec![0xffu8; rows.div_ceil(8)];
        if !matches!(v.validity(), Validity::AllValid) {
            valid.fill(0);
            for i in 0..rows {
                if !v.is_null_at(i) {
                    valid[i / 8] |= 1 << (i % 8);
                }
            }
        }
        let data = v.data().ok_or_else(|| Error::internal("a flattened column is not flat"))?;
        let mut text = Vec::new();
        let values = match data {
            Data::Empty => {
                text = vec![0u128; rows];
                text.as_ptr().cast::<u8>()
            }
            Data::Bool(b) => b.as_slice().as_ptr().cast(),
            Data::Int8(b) => b.as_slice().as_ptr().cast(),
            Data::Int16(b) => b.as_slice().as_ptr().cast(),
            Data::Int32(b) => b.as_slice().as_ptr().cast(),
            Data::Int64(b) => b.as_slice().as_ptr().cast(),
            Data::Int128(b) => b.as_slice().as_ptr().cast(),
            Data::UInt8(b) => b.as_slice().as_ptr().cast(),
            Data::UInt16(b) => b.as_slice().as_ptr().cast(),
            Data::UInt32(b) => b.as_slice().as_ptr().cast(),
            Data::UInt64(b) => b.as_slice().as_ptr().cast(),
            Data::UInt128(b) => b.as_slice().as_ptr().cast(),
            Data::Float32(b) => b.as_slice().as_ptr().cast(),
            Data::Float64(b) => b.as_slice().as_ptr().cast(),
            Data::Varlen(s) => {
                let arena = s.arena();
                text = s
                    .views()
                    .iter()
                    .map(|view| match view.bytes_in(arena) {
                        Some(b) => text::make(b),
                        None => text::make(&[]),
                    })
                    .collect();
                text.as_ptr().cast::<u8>()
            }
            _ => return Err(Error::internal("a column of a type the generator refuses")),
        };
        Ok(Held { values, valid, _text: text, _chunk: std::marker::PhantomData })
    }

    fn col(&self) -> Col {
        Col { values: self.values, valid: self.valid.as_ptr() }
    }
}

/// The root of a scan the first engine runs for us.
struct Scan<'f, 'a>(&'f Feed<'a>);

impl fmt::Debug for Scan<'_, '_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_tuple("Scan").field(self.0).finish()
    }
}

impl Sink for Scan<'_, '_> {
    type Local = ();

    fn local(&self) -> Self::Local {}

    fn parallel(&self) -> bool {
        false
    }

    fn sink(&self, chunk: &Chunk, _local: &mut Self::Local) -> Result<Progress> {
        self.0.push(chunk)?;
        Ok(Progress::More)
    }

    fn combine(&self, _local: Self::Local) -> Result<()> {
        Ok(())
    }

    fn finalize(&self, _threads: &Lease<'_>) -> Result<()> {
        Ok(())
    }
}
