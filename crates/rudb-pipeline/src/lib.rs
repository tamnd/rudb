//! The push operator interface, the pipeline, and the two drivers that run one.
//!
//! Rank 4 in the layer rule. See `xtask/layers.toml` and `spec/18-package-layout.md`.
//!
//! # Why push
//!
//! `rudb-exec` used to have a pull interface: an operator had `next`, and every operator drove its
//! children. Its own doc comment said the scheduler was supposed to know only that a pipeline has a
//! source, some streaming operators and a sink, which is a description of a push interface written
//! above a pull one.
//!
//! The conversion was not a refactor that got cheaper by waiting. It touched every operator, every
//! test and every internal loop that assumed it could block, and the single threaded implementation
//! behind the push interface is [`run_serial`], which is fifty lines with the error handling and
//! about twenty without. So the interface went in first, the implementations behind it moved one at
//! a time, and `rudb-exec` now builds [`Pipeline`] values and hands them to [`run_serial`]. There is
//! no pull left inside the engine. The one at the edge is [`root`], which is what an embedded
//! library owes a caller that owns its own loop.
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
//! # The one seam it does own
//!
//! [`Compaction`] is the `chunk.compaction` seam, and it is here because what happens to a chunk
//! between two operators is exactly what this crate is about. An operator that has narrowed a chunk
//! calls [`narrow`] and the seam decides whether the kept rows are copied out or left as a
//! selection over what they came from. The decision is per chunk, never per row, and the three
//! implementations in the tree disagree about it on purpose.
//!
//! # Measuring without asking the operators to
//!
//! [`Watched`] is a wrapper that goes around any of the three, reading a clock and counting rows on
//! every call. Nothing inside an operator mentions a counter, so an operator written next year is
//! measured the day it is written by somebody who never read that module. It is per call, which is
//! per chunk, which is the granularity rule.
//!
//! It also does not know about threads, and neither does any operator. [`run_parallel`] runs
//! several instances of the same [`Pipeline`] the serial driver runs one of, and the difference
//! between them is the starting and the combining rather than anything an operator can see. What
//! decides how many instances is [`Pipeline::degree`]: the pool's ceiling, whether every operator
//! will run as more than one instance, and how many morsels the source says it has, which is what
//! keeps a query over one chunk on one thread.
//!
//! [`Pool`] is the thread budget and the threads themselves, and it belongs to the database rather
//! than to the query, because two queries on a sixteen core machine should use sixteen threads
//! between them. Its workers park between queries rather than being started per pipeline, and the
//! `unsafe` block in `pool.rs` is what lets a parked thread run work that borrows a plan. Its own
//! documentation has the invariant that makes that sound.
//!
//! [`keep_pages`] is the same idea about memory, which is why it is in this crate and not in one
//! about files or about values. A thread that is made per query costs sixteen microseconds and a
//! page that is faulted in per query costs about the same for every forty of them, so a query that
//! touches forty megabytes of memory it freed at the end of the last one pays a third of its CPU
//! time to the kernel for the privilege. Keeping the threads and keeping the pages are the same
//! sentence said about two resources, and both of them belong to the database rather than to the
//! query.
//!
//! What this crate knows about waiting is that an operator can report [`Progress::Blocked`] for
//! exactly four reasons, which is what makes the wait for graph finite and a deadlock a bug report
//! with the cycle in it rather than a hang. Nothing reports one yet, so neither driver parks and
//! both report it instead.

#![deny(unsafe_code)]

mod compact;
mod dynamic;
mod morsel;
mod pages;
mod parallel;
mod pipeline;
mod pool;
mod progress;
mod root;
mod serial;
mod traits;
mod watch;

#[cfg(test)]
mod tests;

pub use compact::{Compaction, Copied, Gauge, compaction, narrow};
pub use dynamic::{DynSink, DynStream, LocalState};
pub use morsel::Morsel;
pub use pages::keep_pages;
pub use parallel::{Spread, run_parallel};
pub use pipeline::{Locals, Pipeline};
pub use pool::{Lease, Pool};
pub use progress::{Blocked, BlockedReason, BufferId, IoToken, MemoryToken, PipelineId, Progress};
pub use root::{RootPlace, RootReader, RootSink, root, root_in_order};
pub use serial::run_serial;
pub use traits::{Sink, Source, Stream};
pub use watch::Watched;
