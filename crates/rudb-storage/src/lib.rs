//! Blocks, row groups, column chunks, statistics and the buffer manager.
//!
//! Rank 5 in the layer rule. See `xtask/layers.toml` and `spec/18-package-layout.md`.
//!
//! This crate is allowed `unsafe`, per `spec/16-testing.md` section 16.7. Every block carries
//! the invariant that makes it sound, and the lint in the workspace manifest is what turns that
//! into a build failure rather than a habit.

//! What is here today is [`MemoryTable`], which is the M0 answer to where rows live. The format
//! itself is M2 work and it replaces the inside of that type rather than the shape of it.

pub mod memory;

pub use memory::MemoryTable;
