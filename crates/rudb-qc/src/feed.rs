//! Running one pipeline as the steps of section 5.4 of `spec/compiler/05-pipelines-and-state.md`,
//! handing the body one chunk to a morsel.
//!
//! Only the body is generated in C1. The other steps a pipeline has are small and run here: init
//! writes the state header and points the state at what the runtime made for it, the join tables
//! its probes read included, and finalize reads an aggregate's groups out of its table or lays out
//! a join build's table for the pipelines that probe it. Every call of the body returns a
//! [`Status`] and [`Feed::push`] does what it asks.
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
//!
//! Past a join probe one row can make many, so there the buffers start as long as the morsel and
//! the body says `NeedMemory` when they are full. The driver then doubles them and runs the morsel
//! again from the start, which is safe because the only thing such a body changes is the buffers
//! and their count, and both start over.

use std::fmt;
use std::sync::{Arc, Mutex, MutexGuard};

use rudb_common::{Cancel, Error, ErrorCode, Result};
use rudb_pipeline::{Lease, Progress, Sink};
use rudb_plan::Plan;
use rudb_qc_gen::{Body, Out};
use rudb_qc_interp::Program;
use rudb_qc_ir::status::{Kind, Status};
use rudb_qc_ir::{ErrorKind, Module};
use rudb_qc_pipe::{Pipeline, Step};
use rudb_qc_plan::Column;
use rudb_qc_rt::abi::{Col, Morsel, StateHeader};
use rudb_qc_rt::{RUNTIME_ERROR, Rt, text};
use rudb_vector::{Chunk, Data, Validity, Vector};

use crate::Under;
use crate::finish::{self, Cell, cell, vector};

/// One pipeline being run.
pub(crate) struct Feed<'a> {
    module: &'a Module,
    program: &'a Program,
    func: usize,
    body: &'a Body,
    steps: Vec<Step>,
    columns: &'a [Column],
    cancel: Cancel,
    inner: Mutex<Inner<'a>>,
}

/// What a call changes.
struct Inner<'a> {
    rt: &'a mut Rt,
    /// The body's state, in cache lines so that the header is aligned as the spec lays it out.
    state: Vec<Line>,
    /// The chunks a result body produced.
    out: Vec<Chunk>,
    /// Whether the body said the pipeline may stop.
    done: bool,
}

/// One cache line of state.
#[repr(C, align(64))]
#[derive(Clone, Copy)]
struct Line([u8; 64]);

impl fmt::Debug for Feed<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Feed").field("func", &self.body.func).finish_non_exhaustive()
    }
}

impl<'a> Feed<'a> {
    /// A feed for pipeline `p`, whose generated body is `body`, with its init step run.
    pub(crate) fn new(
        module: &'a Module,
        program: &'a Program,
        p: &'a Pipeline,
        body: &'a Body,
        rt: &'a mut Rt,
        cancel: &Cancel,
    ) -> Result<Feed<'a>> {
        let func = program
            .func(&body.func)
            .ok_or_else(|| Error::internal(format!("no function {} in the module", body.func)))?;
        let steps = p.steps();
        let columns = match &p.sink {
            rudb_qc_pipe::Sink::Result { columns, .. }
            | rudb_qc_pipe::Sink::Build { columns, .. }
            | rudb_qc_pipe::Sink::Aggregate { columns, .. } => columns.as_slice(),
        };
        let mut state = vec![Line([0; 64]); (body.state as usize).div_ceil(64).max(1)];
        // Init. With one worker the local state is the shared state, so the header points at its
        // own block, which never moves because the vector is never grown.
        let header = StateHeader::new(state.as_ptr().cast());
        // SAFETY: the first line of the state is 64 bytes aligned to 64, which is the header.
        unsafe { state.as_mut_ptr().cast::<StateHeader>().write(header) };
        if let Out::Aggregate(g) = &body.sink
            && let Some(at) = g.row
        {
            // The one group of an aggregate with no groups was made with the table, and rows
            // never move, so its address is written once.
            let table = rt
                .table(g.table)
                .ok_or_else(|| Error::internal("the aggregate's table is not in the runtime"))?;
            let row = table.address(0) as u64;
            bytes(&mut state)[at as usize..at as usize + 8].copy_from_slice(&row.to_le_bytes());
        }
        for probe in &body.probes {
            // The build ran and was finalized before this pipeline started, and its table does
            // not move after that.
            let table = rt
                .join(probe.table)
                .filter(|t| t.is_finished())
                .ok_or_else(|| Error::internal("a probe of a join table that is not built"))?;
            let published = table.published();
            let st = bytes(&mut state);
            for (at, word) in [
                (probe.directory, published.directory as u64),
                (probe.shift, published.shift),
                (probe.tags, published.tags as u64),
            ] {
                st[at as usize..at as usize + 8].copy_from_slice(&word.to_le_bytes());
            }
        }
        let cancel = cancel.clone();
        Ok(Feed {
            module,
            program,
            func,
            body,
            steps,
            columns,
            cancel,
            inner: Mutex::new(Inner { rt, state, out: Vec::new(), done: false }),
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

    /// Runs the body over one chunk, and says whether the pipeline wants more.
    pub(crate) fn push(&self, chunk: &Chunk) -> Result<Progress> {
        let chunk = chunk.clone().settled()?.into_flat()?;
        let rows = chunk.len();
        if rows == 0 {
            return Ok(Progress::More);
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
        if inner.done {
            return Ok(Progress::Done);
        }
        let mut room = rows;
        let mut buffers = Vec::new();
        let Inner { rt, state, out, done } = &mut *inner;
        'attempt: loop {
            buffers.clear();
            if let Out::Result { count, columns, capacity } = &self.body.sink {
                let st = bytes(state);
                st[*count as usize..*count as usize + 8].fill(0);
                if let Some(at) = capacity {
                    st[*at as usize..*at as usize + 8]
                        .copy_from_slice(&(room as u64).to_le_bytes());
                }
                for slot in columns {
                    let mut b = Buffers {
                        values: vec![0u128; (room * slot.ty.bytes() as usize).div_ceil(16)],
                        valid: vec![0u8; room],
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
            let st = state.as_mut_ptr().cast::<u8>();
            loop {
                let status =
                    self.program.call(self.func, st, (&raw const morsel).cast(), &mut **rt);
                match Status(status).kind() {
                    Kind::Ok => break 'attempt,
                    // The body saved where it got to in the header's cursor and picks up there.
                    Kind::Yield => {}
                    Kind::Done => {
                        *done = true;
                        break 'attempt;
                    }
                    // Only a result sink past a probe asks, and only when its buffers are full.
                    Kind::NeedMemory
                        if matches!(self.body.sink, Out::Result { capacity: Some(_), .. }) =>
                    {
                        room = room.checked_mul(2).ok_or_else(|| {
                            Error::internal("a join made more rows than memory holds")
                        })?;
                        continue 'attempt;
                    }
                    _ => return Err(self.check(Status(status), rt)),
                }
            }
        }
        drop(held);
        if let Out::Result { count, columns, .. } = &self.body.sink {
            let st = bytes(state);
            let n = u64::from_le_bytes(
                st[*count as usize..*count as usize + 8].try_into().unwrap_or_default(),
            );
            let n = usize::try_from(n).unwrap_or(usize::MAX).min(room);
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
        Ok(if *done { Progress::Done } else { Progress::More })
    }

    /// Runs the steps after the body once every chunk has been pushed, and returns the rows the
    /// pipeline produced.
    pub(crate) fn finish(self) -> Result<Vec<Chunk>> {
        let inner = self.inner.into_inner().map_err(|_| Error::internal("a feed was poisoned"))?;
        let mut out = inner.out;
        for step in &self.steps {
            match step {
                Step::Init | Step::Body => {}
                // The accumulators of an aggregate with no groups are the one row of its table,
                // so there is nothing kept aside to flush.
                Step::LocalFin => {}
                Step::Merge => return Err(Error::internal("a merge step with one worker")),
                Step::Finalize => match &self.body.sink {
                    Out::Aggregate(g) => out = finish::groups(inner.rt, g, self.columns)?,
                    // The build publishes its table to the probes and produces no rows.
                    Out::Build(b) => {
                        inner.rt.finish_join(b.table)?;
                        out = Vec::new();
                    }
                    Out::Result { .. } => {}
                },
            }
        }
        Ok(out)
    }

    fn lock(&self) -> MutexGuard<'_, Inner<'a>> {
        self.inner.lock().unwrap_or_else(|held| held.into_inner())
    }

    /// The error a status that stops the pipeline stands for.
    fn check(&self, s: Status, rt: &mut Rt) -> Error {
        match s.kind() {
            Kind::Cancelled => match self.cancel.check() {
                Err(e) => e,
                Ok(()) => Error::interrupt("Interrupted!"),
            },
            Kind::Error if s.payload() == RUNTIME_ERROR => rt
                .take_error()
                .unwrap_or_else(|| Error::internal("the runtime failed and did not say why")),
            Kind::Error => {
                let Some(site) =
                    usize::try_from(s.payload()).ok().and_then(|at| self.module.errors.get(at))
                else {
                    return Error::internal(format!("status {s:?} names no error site"));
                };
                let code = match site.kind {
                    ErrorKind::Overflow | ErrorKind::DivideByZero | ErrorKind::OutOfRange => {
                        ErrorCode::OutOfRange
                    }
                    ErrorKind::Conversion => ErrorCode::Conversion,
                    ErrorKind::Cancel => ErrorCode::Interrupt,
                    ErrorKind::Internal => return Error::internal(site.text.clone()),
                };
                Error::new(code, site.text.clone())
            }
            // No body in C1 has a guard, and only a result sink past a probe grows, so these are
            // bugs.
            Kind::Deopt => Error::internal(format!("a pipeline deoptimized at {s:?}")),
            Kind::NeedMemory => Error::internal(format!("a pipeline asked for memory, {s:?}")),
            _ => Error::internal(format!("a pipeline returned status {s:?}")),
        }
    }
}

/// The output buffers of one result column for one call.
struct Buffers {
    values: Vec<u128>,
    valid: Vec<u8>,
}

/// The state as bytes.
fn bytes(state: &mut [Line]) -> &mut [u8] {
    // SAFETY: a `Line` is 64 bytes with no padding and any byte pattern is one.
    unsafe { std::slice::from_raw_parts_mut(state.as_mut_ptr().cast::<u8>(), state.len() * 64) }
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
        self.0.push(chunk)
    }

    fn combine(&self, _local: Self::Local) -> Result<()> {
        Ok(())
    }

    fn finalize(&self, _threads: &Lease<'_>) -> Result<()> {
        Ok(())
    }
}
