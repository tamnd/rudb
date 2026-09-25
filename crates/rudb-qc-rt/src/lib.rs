//! The runtime compiled pipelines call, per `spec/compiler/05-pipelines.md` and section 6.5 of
//! `spec/compiler/06-qir.md`: the morsel and state ABI, `str16` strings and the heap they live in,
//! `LIKE` patterns, the aggregation and join hash tables, and [`Rt`], which implements every
//! function in the catalogue for whichever tier runs the code.

#![deny(unsafe_code)]

pub mod abi;
pub mod join;
pub mod like;
mod mem;
mod rt;
pub mod table;
pub mod text;

pub use rt::{Kernel, RUNTIME_ERROR, Rt, hash};
