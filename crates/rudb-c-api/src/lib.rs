//! A libduckdb-compatible C surface, plus a native C surface.
//!
//! Rank 15 in the layer rule. See `xtask/layers.toml` and `spec/18-package-layout.md`.
//!
//! This crate is allowed `unsafe`, per `spec/16-testing.md` section 16.7. Every block carries
//! the invariant that makes it sound, and the lint in the workspace manifest is what turns that
//! into a build failure rather than a habit.

/// The crate this rank belongs to, so that the layer check has something to read and the
/// scaffold compiles. Replaced by the first real item.
pub const RANK: u8 = 15;
