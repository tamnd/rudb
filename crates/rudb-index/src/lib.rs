//! The key index of `notes/Spec/2140/engine-v4/11-indexes-and-keys.md`.
//!
//! An index maps a normalized key to the rid of a row that had it. It is an LSM: the newest
//! entries in memory, and below them sorted immutable runs, each with the first key of every block
//! and a bloom filter kept in memory, so a lookup in a run tests the filter, finds one block by its
//! fences and reads that block.
//!
//! An entry is a hint and never the truth (section 11.3). It says that the row at `rid` had the key
//! at `ts`, and whoever finds it checks the row. That is why nothing here ever removes or changes
//! an entry when a row goes or moves: a newer entry is added, and the old one fails the check until
//! a merge drops it.
//!
//! This crate knows what a key and a rid are and nothing about a file, a table or a transaction.
//! [`key`] writes keys, [`l0`] holds the newest entries in a B+tree under optimistic lock coupling,
//! and [`run`] holds runs and writes them as `RUDBKI1`.

pub mod key;
pub mod l0;
pub mod run;

mod bloom;

pub use key::normalize;
pub use l0::L0;
pub use run::{Hit, Run, RunWriter};
