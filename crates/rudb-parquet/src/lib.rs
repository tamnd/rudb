//! The Parquet reader and writer, with page skipping and dictionary passthrough.
//!
//! Rank 5 in the layer rule. See `xtask/layers.toml` and `spec/18-package-layout.md`.
//!
//! The read path is here end to end: the footer in [`Metadata`], the page headers, the run length
//! and bit packed hybrid that carries the levels and the dictionary indices, the plain encoding, and
//! the assembly that puts values back where the levels say they belong. [`Reader`] is what a scan
//! drives, and it produces `rudb-vector` chunks a row group at a time with only the projected
//! columns read.
//!
//! Nothing here writes a file. The write path is M2m and it belongs next to a compressor, which
//! `rudb-compress` does not have yet, because a compressor nothing writes with is a compressor
//! nothing tests.
//!
//! # What a file has to be for this to read it
//!
//! A flat schema. Nested types are the format's own recursion and reading them needs repetition
//! levels, which this does not decode, so the footer reader rejects a group column by name rather
//! than quietly producing a column that is wrong.
//!
//! `UNCOMPRESSED` or `SNAPPY` pages. Everything else says so by name, which is `rudb-compress`
//! being honest about what it has rather than this guessing.
//!
//! `PLAIN` or dictionary encoded values. `DELTA_BINARY_PACKED`, `DELTA_BYTE_ARRAY`,
//! `DELTA_LENGTH_BYTE_ARRAY` and `BYTE_STREAM_SPLIT` are named in the error when a file uses one.
//! They are the encodings a v2 writer reaches for and they are the next thing to add, and the
//! arithmetic for all four is already in `rudb-encoding` for rudb's own format.

#![forbid(unsafe_code)]

mod column;
mod hybrid;
mod metadata;
mod page;
mod plain;
mod reader;
mod thrift;

pub use metadata::{ColumnChunk, Encoding, Metadata, Physical, RowGroup, SchemaColumn, Stats};
pub use reader::{Reader, read};

/// The codec a column chunk's pages are compressed with.
///
/// Re-exported rather than redefined. `rudb-compress` numbers its codecs the way Parquet's metadata
/// numbers them, because Parquet's list is the list everyone else copied, and a second enum here
/// would be a second table to keep in step with the first.
pub use rudb_compress::Codec;
