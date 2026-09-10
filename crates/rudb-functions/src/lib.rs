//! The scalar, aggregate and window function library.
//!
//! Rank 7 in the layer rule. See `xtask/layers.toml` and `spec/18-package-layout.md`.
//!
//! What is here today is the signature half: given a name and the types of the arguments, which
//! function is that and what does it return. The implementations are separate work and they go
//! behind the same names, so the binder does not change when they arrive.

#![forbid(unsafe_code)]

pub mod signature;

pub use signature::{FunctionKind, Resolved, kind_of, resolve};
