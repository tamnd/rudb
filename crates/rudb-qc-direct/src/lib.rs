//! `direct`, the query compiler's single pass backend: it turns a QIR function into x86-64 machine
//! code with no dependencies, per section 8.5 of `spec/compiler/08-backends.md`.
//!
//! It is the tier that has to make the first morsel fast, so it does in two linear passes what
//! `clif` does in a pipeline of them. The analysis pass in [`analysis`] finds the loops, lays out
//! the blocks and computes liveness as intervals over that layout. The code generation pass then
//! selects instructions, allocates registers and encodes in one walk over the blocks, writing
//! through the encoder in [`asm`].
//!
//! Like `clif`, the output is bytes and relocations and nothing else. The code arena and the
//! runtime entries are in `rudb-qc-rt` above this crate, and compiling is a pure function of the
//! QIR, which is why the crate forbids `unsafe`.

#![forbid(unsafe_code)]

pub mod analysis;
pub mod asm;
