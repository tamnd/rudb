//! The Parquet reader and writer, with page skipping and dictionary passthrough.
//!
//! Rank 5 in the layer rule. See `xtask/layers.toml` and `spec/18-package-layout.md`.
//!
//! The read path is here end to end, in four layers that stack.
//!
//! [`Metadata`] reads the footer, which is the half of a Parquet reader that decides how much of
//! the file the other half has to touch, because the footer says where every column chunk of every
//! row group is, and a query over one column of `hits` reads a hundred and five entries of the
//! footer and then one column's bytes. [`Pages`] walks a chunk those bytes came back as,
//! decompressing each page and handing back its header, and [`Page::definitions`] reads the levels
//! that say which of its rows are null. [`Page::into_vector`] turns a page's body into a vector.
//! [`Reader`] is the one a scan drives: it picks the columns, walks the row groups, and puts the
//! columns side by side into chunks.
//!
//! Nothing here writes a file. The write path is M2m and it belongs next to a compressor, which
//! `rudb-compress` does not have yet, because a compressor nothing writes with is a compressor
//! nothing tests.
//!
//! # What a file has to be for this to read it
//!
//! A flat schema. Nested types are the format's own recursion and reading them needs repetition
//! levels, which nothing here decodes, so the footer reader refuses a group column by name rather
//! than quietly handing back a column that is wrong.
//!
//! `UNCOMPRESSED` or `SNAPPY` pages. Everything else says so by name, which is `rudb-compress`
//! being honest about what it has rather than this crate guessing.
//!
//! `PLAIN` or dictionary encoded values. The delta encodings and `BYTE_STREAM_SPLIT` are named in
//! the error when a file uses one, and so are `INT96`, fixed length byte arrays, and byte arrays
//! whose bytes are not text. See `spec/engine/05-scan.md` sections 5.3 to 5.6 and the checklist on
//! the sub-milestone issue.

#![forbid(unsafe_code)]

mod chunk;
mod delta;
mod hybrid;
mod metadata;
mod page;
mod reader;
mod thrift;
mod values;

pub use chunk::{Page, Pages};
pub use metadata::{ColumnChunk, Encoding, Metadata, Physical, RowGroup, SchemaColumn, Stats};
pub use page::{Body, DataV1, DataV2, Dictionary, Header};
pub use reader::{Reader, read};
