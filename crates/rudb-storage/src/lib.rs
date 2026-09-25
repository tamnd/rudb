//! Blocks, row groups, column chunks, statistics and the buffer manager.
//!
//! Rank 5 in the layer rule. See `xtask/layers.toml` and `spec/18-package-layout.md`.
//!
//! This crate is allowed `unsafe`, per `spec/16-testing.md` section 16.7. Every block carries
//! the invariant that makes it sound, and the lint in the workspace manifest is what turns that
//! into a build failure rather than a habit.

//! What is here today is [`MemoryTable`], which is the M0 answer to where rows live. The format
//! itself is M2 work and it replaces the inside of that type rather than the shape of it.

pub mod arena;
pub mod count;
pub mod deletes;
pub mod hot;
pub mod memory;
pub mod sieve;
pub mod tally;
pub mod undo;
pub mod zone;

pub use count::{Counting, Counts, Partial};
pub use deletes::{DeleteVector, Refusal};
pub use hot::{HotStripe, Lease, Width};
pub use memory::MemoryTable;
pub use sieve::{Blocked, Sieve};
pub use tally::{TALLY_VALUES, Tally};
pub use undo::{UndoBuffer, UndoSpace};
pub use zone::{Probe, Range, Zone};
