//! Named, swappable implementations of the mechanisms the literature disagrees about.
//!
//! Rank 2 in the layer rule. See `xtask/layers.toml` and `spec/18-package-layout.md`.
//!
//! # What a seam is
//!
//! A seam is a point in the engine where two or more published designs disagree about how to do
//! the same thing. A hash table is a seam. A type is not: there is one right answer for what
//! `DECIMAL(18,3) + INTEGER` produces and it is DuckDB's answer, so nothing about it is swappable.
//!
//! A seam has six parts and all six are mandatory. A trait with a narrow interface. A registry of
//! named implementations. Exactly one implementation marked as the reference. At least two
//! implementations in the tree. A policy that picks one. And a place in `EXPLAIN` where the choice
//! is printed. This crate is the second, third, fifth and part of the sixth. The first is the
//! seam's own trait, which lives in the lowest crate that needs it, and the fourth is the work.
//!
//! # Why it is worth a crate
//!
//! So that somebody who reads a paper on Monday has a number on Friday. They write one file, add
//! one line to a `register.rs`, and the differential corpus runs their implementation against the
//! reference on the next `cargo test` without them writing a test. Then
//! `rudb-bench sweep --seam <seam>` runs a whole suite once per registered implementation with
//! everything else held fixed, which is the part that makes the result publishable, because the
//! commonest failure in this kind of comparison is that the new thing was measured against a
//! different build, machine or dataset.
//!
//! # The rule that makes it affordable
//!
//! A seam is crossed once per chunk, never once per row. Every seam trait's methods take a whole
//! chunk, a whole column, a whole morsel, a whole partition, or a decision made at plan time.
//! None of them take a row, a value or a single key. An indirect call every 122,880 rows costs
//! nothing measurable and an indirect call per row costs an order of magnitude, and that
//! difference is the entire reason a design with this many swappable parts can be fast at all.
//!
//! The seam is at the door of the loop, not inside it. Inside, an implementation is a
//! monomorphised loop over primitives with no dynamic dispatch in it anywhere.
//!
//! # Using one
//!
//! ```
//! use rudb_seam::{Context, Determinism, Provenance, Registry, SeamId, Settings, Strategy};
//!
//! // A seam trait names `Strategy` as a supertrait and adds the narrow interface.
//! trait Greeter: Strategy {
//!     fn greet(&self) -> String;
//! }
//!
//! #[derive(Debug)]
//! struct Plain;
//!
//! impl Strategy for Plain {
//!     fn name(&self) -> &'static str { "plain" }
//!     fn describe(&self) -> &'static str { "says hello" }
//!     fn provenance(&self) -> Provenance { Provenance::Reference }
//!     fn applicable(&self, _context: &Context<'_>) -> bool { true }
//! }
//!
//! impl Greeter for Plain {
//!     fn greet(&self) -> String { "hello".to_string() }
//! }
//!
//! let registry: Registry<dyn Greeter> =
//!     Registry::<dyn Greeter>::builder(SeamId::HashTable).reference(Box::new(Plain)).build();
//!
//! let settings = Settings::new();
//! let context = Context::new(SeamId::HashTable, &settings);
//! let chosen = registry.choose(&context).unwrap();
//!
//! assert_eq!(chosen.greet(), "hello");
//! assert_eq!(chosen.deterministic(), Determinism::Exact);
//! ```

#![forbid(unsafe_code)]

mod context;
mod policy;
mod registry;
mod seam;
mod settings;
mod strategy;

#[cfg(test)]
mod tests;

pub use context::Context;
pub use policy::{ChoiceReason, Policy, PolicyMode};
pub use registry::{Choice, Registries, Registry, RegistryBuilder, RegistryView, StrategyRow};
pub use seam::SeamId;
pub use settings::{SEAM_PREFIX, Settings};
pub use strategy::{Determinism, Provenance, Strategy};
