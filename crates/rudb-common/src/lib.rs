//! Types, values, errors, arenas and hashing. The bottom of the workspace.
//!
//! Rank 0 in the layer rule. See `xtask/layers.toml` and `spec/18-package-layout.md`.

#![forbid(unsafe_code)]

/// The crate this rank belongs to, so that the layer check has something to read and the
/// scaffold compiles. Replaced by the first real item.
pub const RANK: u8 = 0;
