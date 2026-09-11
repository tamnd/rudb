//! The Parquet reader and writer, with page skipping and dictionary passthrough.
//!
//! Rank 5 in the layer rule. See `xtask/layers.toml` and `spec/18-package-layout.md`.
//!
//! What is here so far reaches as far as a page. [`Metadata`] reads the footer, which is the half
//! of a Parquet reader that decides how much of the file the other half has to touch, because the
//! footer says where every column chunk of every row group is, and a query over one column of
//! `hits` reads a hundred and five entries of the footer and then one column's bytes. [`Pages`]
//! then walks a chunk those bytes came back as, decompressing each page and handing back its
//! header, and [`Page::definitions`] reads the levels that say which of its rows are null.
//!
//! What is not here yet is the values. A page's body past its levels is still bytes, because the
//! encodings are the next change and a page decoder that landed with all of them at once is a
//! change nobody can review. See `spec/engine/05-scan.md` sections 5.3 to 5.6 and the checklist on
//! the sub-milestone issue.

#![forbid(unsafe_code)]

mod chunk;
mod delta;
mod hybrid;
mod metadata;
mod page;
mod thrift;
mod values;

pub use chunk::{Page, Pages};
pub use metadata::{ColumnChunk, Encoding, Metadata, Physical, RowGroup, SchemaColumn, Stats};
pub use page::{Body, DataV1, DataV2, Dictionary, Header};
