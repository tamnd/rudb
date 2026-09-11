//! The Parquet reader and writer, with page skipping and dictionary passthrough.
//!
//! Rank 5 in the layer rule. See `xtask/layers.toml` and `spec/18-package-layout.md`.
//!
//! What is here so far is the footer: the Thrift compact protocol decoder that reads it and the
//! structures it decodes into. That is the half of a Parquet reader that decides how much of the
//! file the other half has to touch, because the footer says where every column chunk of every row
//! group is, and a query over one column of `hits` reads a hundred and five entries of the footer
//! and then one column's bytes. Nothing in here reads a page yet.
//!
//! The rest of 2d is the page decoders, Snappy and the scan that turns pages into chunks. See
//! `spec/engine/05-scan.md` sections 5.3 to 5.6 and the checklist on the sub-milestone issue.

#![forbid(unsafe_code)]

mod metadata;
mod thrift;

pub use metadata::{
    ColumnChunk, Compression, Encoding, Metadata, Physical, RowGroup, SchemaColumn, Stats,
};
