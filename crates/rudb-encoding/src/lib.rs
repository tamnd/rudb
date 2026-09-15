//! Every encoding, the cascade machinery, the cost model and multi-column detection.
//!
//! Rank 2 in the layer rule. See `xtask/layers.toml` and `spec/18-package-layout.md`.
//!
//! [`bitpack`] is the bottom of every integer encoding in `spec/06-compression.md` section 6.2 and
//! the thing FOR, DELTA and DICT all end in. [`integer`] is those encodings and the cascade over
//! them from section 6.3, which is where the ratios actually are. [`fsst`] is one string against
//! one symbol table and [`string`] is a column of them, which is where most of ClickBench `hits`
//! lives. [`sketch`] is how a write path answers a question about a column it cannot hold in
//! memory, which is where every decision in sections 6.4 and 6.5 starts. [`chooser`] is the search
//! over all of that, held apart from the encodings themselves so that how long the writer is willing
//! to spend deciding is a knob rather than a property of the format.

#![forbid(unsafe_code)]

pub mod bitpack;
pub mod chooser;
pub mod integer;
mod lz;
pub mod multi;
mod reader;
pub mod sketch;
pub mod string;

/// The symbol table and the code, which live a layer down now that a vector can be in FSST form.
///
/// They were written here, because this is where compression is. They moved to `rudb-vector` when
/// the vector gained the form, because a vector that holds FSST codes has to be able to read one and
/// this crate is above it in the layer rule. What stayed here is everything that decides to use it:
/// [`string`] trains a table on a column and picks between this and the other string encodings, and
/// [`multi`] looks for one table that suits several columns.
///
/// The re-export is so that a caller that had `rudb_encoding::fsst::SymbolTable` still has it. There
/// is one implementation and it is over there.
pub use rudb_vector::fsst;
