//! MVCC versions, the write-ahead log and checkpointing.
//!
//! Rank 6 in the layer rule. See `xtask/layers.toml` and `spec/18-package-layout.md`.

#![forbid(unsafe_code)]

pub mod log;
