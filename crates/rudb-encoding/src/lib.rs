//! Every encoding, the cascade machinery, the cost model and multi-column detection.
//!
//! Rank 2 in the layer rule. See `xtask/layers.toml` and `spec/18-package-layout.md`.
//!
//! [`bitpack`] is the bottom of every integer encoding in `spec/06-compression.md` section 6.2 and
//! the thing FOR, DELTA and DICT all end in. [`integer`] is those encodings and the cascade over
//! them from section 6.3, which is where the ratios actually are.

#![forbid(unsafe_code)]

pub mod bitpack;
pub mod integer;
