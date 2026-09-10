//! Types, values and errors. The bottom of the workspace.
//!
//! Rank 0 in the layer rule, which means everything can see this crate and this crate can see
//! nothing. See `xtask/layers.toml` and `spec/18-package-layout.md`.
//!
//! What lives here is the vocabulary every other crate spells its signatures in. A type, a single
//! value, an error and a span into the query text that produced it. Nothing here knows what a
//! vector is, what a plan is or what a file is, and keeping it that way is what stops rank 0 from
//! becoming a second name for the whole database.

#![forbid(unsafe_code)]

pub mod error;
pub mod settings;
pub mod types;
pub mod value;

pub use error::{Error, ErrorCode, Result, Span};
pub use settings::{DefaultOrder, NullOrder, Settings};
pub use types::{Field, LogicalType, MAX_DECIMAL_WIDTH, PhysicalType};
pub use value::{Value, civil_from_days, days_from_civil};
