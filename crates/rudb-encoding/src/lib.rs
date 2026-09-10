//! Every encoding, the cascade machinery, the cost model and multi-column detection.
//!
//! Rank 2 in the layer rule. See `xtask/layers.toml` and `spec/18-package-layout.md`.
//!
//! What exists so far is [`bitpack`], which is the bottom of every integer encoding in
//! `spec/06-compression.md` section 6.2 and the thing FOR, DELTA and DICT all end in.

#![forbid(unsafe_code)]

pub mod bitpack;
