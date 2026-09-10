//! Every encoding, the cascade machinery, the cost model and multi-column detection.
//!
//! Rank 2 in the layer rule. See `xtask/layers.toml` and `spec/18-package-layout.md`.
//!
//! [`bitpack`] is the bottom of every integer encoding in `spec/06-compression.md` section 6.2 and
//! the thing FOR, DELTA and DICT all end in. [`integer`] is those encodings and the cascade over
//! them from section 6.3, which is where the ratios actually are. [`fsst`] is one string against
//! one symbol table and [`string`] is a column of them, which is where most of ClickBench `hits`
//! lives. [`sketch`] is how a write path answers a question about a column it cannot hold in
//! memory, which is where every decision in sections 6.4 and 6.5 starts.

#![forbid(unsafe_code)]

pub mod bitpack;
pub mod fsst;
pub mod integer;
pub mod sketch;
pub mod string;
