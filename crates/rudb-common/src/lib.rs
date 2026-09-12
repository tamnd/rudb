//! Types, values and errors. The bottom of the workspace.
//!
//! Rank 0 in the layer rule, which means everything can see this crate and this crate can see
//! nothing. See `xtask/layers.toml` and `spec/18-package-layout.md`.
//!
//! What lives here is the vocabulary every other crate spells its signatures in. A type, a single
//! value, an error, a span into the query text that produced it, and the token that says a running
//! query should stop. Nothing here knows what a vector is, what a plan is or what a file is, and
//! keeping it that way is what stops rank 0 from becoming a second name for the whole database.
//!
//! [`Cancel`] is here for the layer rule rather than because it is a kind of value. The thing that
//! sets it is the embedding API at rank 13 and the thing that reads it is the executor at rank 12,
//! so the only place both can see it from is the bottom. The [`slow`] counter is here for the same
//! reason: the two things that increment it are at rank 1 and rank 3, the thing that reads it is at
//! rank 4, and no two of those three can see each other.

#![forbid(unsafe_code)]

pub mod cancel;
pub mod error;
pub mod memory;
pub mod slow;
pub mod types;
pub mod value;

pub use cancel::Cancel;
pub use error::{Error, ErrorCode, Result, Span};
pub use memory::{ALLOCATION, Memory, Reservation, human};
pub use slow::{Cause, Tally};
pub use types::{Field, LogicalType, MAX_DECIMAL_WIDTH, PhysicalType};
pub use value::{Value, civil_from_days, days_from_civil, interval_micros};
