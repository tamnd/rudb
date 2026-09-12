//! Vectors, physical forms, validity, selection vectors and the string representation.
//!
//! Rank 1 in the layer rule. See `xtask/layers.toml` and `spec/18-package-layout.md`.
//!
//! This is the interface `spec/07-execution.md` section 7.1 calls the widest one in the system.
//! Every operator in every later crate is written against it, which is why it is specified before
//! the first operator exists and changed by RFC afterwards rather than by whoever needs it changed.
//!
//! Six pieces:
//!
//! - [`Vector`], a typed run of at most [`VECTOR_SIZE`] values in one of five physical forms.
//! - [`Chunk`], some vectors of the same length, which is what one operator hands the next.
//! - [`Validity`], which is three cases rather than a bitmap, because knowing there are no nulls is
//!   worth a measurable amount and costs one branch per vector to know.
//! - [`Selection`], which is what a filter produces instead of compacting.
//! - [`StringView`] and [`StringColumn`], the 16 byte string with the 4 byte prefix.
//! - [`Buffer`], the run of values behind a flat vector, which is where the buffer manager arrives.
//!
//! Next to them is [`for_each_layout`], which is the list of physical layouts a kernel writes its
//! loop against. It is here rather than in the kernels because the list is a property of [`Data`],
//! and a kernel that keeps its own copy of it is a kernel that will one day be missing a type.
//!
//! # What is deliberately not here
//!
//! Encoded vectors are M3 work, not because they are hard but because they only pay off alongside
//! the specialization contract that decides when to decode. Borrowed buffers are M2 work, because
//! there is no buffer manager to borrow from. Nested storage is M2 work for the same reason. Each
//! of these is a place the interface will grow, and each one is named here so that growing it is a
//! decision somebody makes rather than something that happens.
//!
//! # Unsafe
//!
//! This crate is on the list in `spec/16-testing.md` section 16.7 that is allowed `unsafe`, and it
//! does not use any yet. The safe version is the baseline every unsafe version has to beat on a
//! benchmark before it lands, so writing it first is not a detour.
//!
//! The lint below is `deny` rather than `forbid` for exactly that reason. Denied means an unsafe
//! block needs an `allow` written next to it, which is a line a reviewer sees. Forbidden would mean
//! the first genuinely faster kernel has to start by editing this file, and a rule that gets
//! deleted the first time it is inconvenient was never a rule.

#![deny(unsafe_code)]

pub mod buffer;
pub mod chunk;
mod layout;
pub mod selection;
pub mod string;
pub mod validity;
pub mod vector;

pub use buffer::{Buffer, Pin};
pub use chunk::Chunk;
pub use selection::Selection;
pub use string::{INLINE_LIMIT, StringColumn, StringView};
pub use validity::{Bitmap, Validity};
pub use vector::{Data, Form, VECTOR_SIZE, Vector};
