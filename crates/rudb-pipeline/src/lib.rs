//! The push operator interface, the pipeline, and the driver that runs one on a single thread.
//!
//! Rank 4 in the layer rule. See `xtask/layers.toml` and `spec/18-package-layout.md`.
//!
//! # Why push, and why now
//!
//! `rudb-exec` has a pull interface: an operator has `next`, and every operator drives its
//! children. Its own doc comment says the scheduler is supposed to know only that a pipeline has a
//! source, some streaming operators and a sink, which is a description of a push interface written
//! above a pull one.
//!
//! The conversion is not a refactor that gets cheaper by waiting. It touches every operator, every
//! test and every internal loop that assumed it could block, and the single threaded
//! implementation behind the push interface is [`run_serial`], which is fifty lines with the
//! error handling and about twenty without. So the interface is final now and the implementations
//! behind it move one at a time.
//!
//! # The three traits
//!
//! [`Source`] produces chunks, [`Stream`] transforms them in place, and [`Sink`] consumes them
//! into state. All three take `&self` rather than `&mut self`, with per instance mutable state
//! passed in separately, which is what lets one pipeline be instantiated on thirty two threads
//! without thirty two copies of every operator's configuration. It is also what forces the state
//! that matters to be named, which is the part that pays for itself when that state has to be
//! spilled, merged or serialised later.
//!
//! [`Sink::combine`] takes the local state by value. Consuming it is what makes merging one
//! thread's partial aggregate twice impossible.
//!
//! # What this crate deliberately does not know
//!
//! Schemas, plans, bindings and types. A pipeline here is plumbing over chunks, and the crate that
//! knows what the columns mean builds one and hands it over. That is what keeps this at rank 4,
//! where it can be used and tested with a source made of two literal chunks and no planner
//! anywhere in the picture.
//!
//! It also does not know about threads. The scheduler is F4. What it knows is that an operator can
//! report [`Progress::Blocked`] for exactly four reasons, which is what makes the wait for graph
//! finite and a deadlock a bug report with the cycle in it rather than a hang.

#![forbid(unsafe_code)]

mod dynamic;
mod morsel;
mod pipeline;
mod progress;
mod root;
mod serial;
mod traits;

#[cfg(test)]
mod tests;

pub use dynamic::{DynSink, DynStream, LocalState};
pub use morsel::Morsel;
pub use pipeline::{Locals, Pipeline};
pub use progress::{Blocked, BlockedReason, BufferId, IoToken, MemoryToken, PipelineId, Progress};
pub use root::{RootReader, RootSink, root};
pub use serial::run_serial;
pub use traits::{Sink, Source, Stream};
