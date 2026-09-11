//! The CSV reader and writer, including dialect sniffing and type inference.
//!
//! Rank 5 in the layer rule. See `xtask/layers.toml` and `spec/18-package-layout.md`.
//!
//! A CSV file says nothing about itself. The delimiter, the quote, whether the first line names the
//! columns and what type each column holds are all conventions, and a reader that has to be told
//! them is a reader every loader script in the world has to be rewritten for. So this sniffs, the
//! way DuckDB does, and every rule in it was read off duckdb v1.4.1 rather than reasoned about.
//! Where the two differ, the difference is written down next to the code, because a sniffer that
//! quietly disagrees produces a column of the wrong type and that is a wrong answer rather than a
//! slow query.
//!
//! Three pieces. [`scan`] turns bytes into fields, [`dialect`] works out the punctuation by running
//! the scanner under each candidate and seeing which one is consistent, and [`infer`] walks a ladder
//! of types to find the first one every value in a column fits. [`Reader`] is the three of them over
//! a file, handing back chunks.
//!
//! [`combine`] is the fourth, and it is there because a read can cover more than one file.
//! `read_csv('data/*.csv')` sniffs every file the pattern matched and combines the answers, since a
//! file that says nothing about itself cannot be the one file whose word is taken the way a Parquet
//! footer is.
//!
//! There is no writer yet. `COPY t TO 'out.csv'` is the statement that wants one and it is not
//! bound, so a writer here would be a writer nothing calls.

#![forbid(unsafe_code)]

pub mod combine;
pub mod dialect;
pub mod infer;
pub mod reader;
pub mod scan;

pub use combine::{across, mismatch, widen};
pub use dialect::{Dialect, Given};
pub use reader::Reader;

/// The crate this rank belongs to, so that the layer check has something to read.
pub const RANK: u8 = 5;
