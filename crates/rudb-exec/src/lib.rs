//! Operators, morsels, the scheduler, hash tables, sorting and spilling.
//!
//! Rank 12 in the layer rule. See `xtask/layers.toml` and `spec/18-package-layout.md`.
//!
//! This is tier 0 of `spec/08-codegen.md` section 8.1: a pull based tree of operators, one variant
//! per logical operator the binder can produce, every one of them written the simplest way that is
//! correct. Tier 0 is never removed and never optional, because it is the reference every faster
//! tier is differentially tested against, and a reference that is clever is a reference nobody can
//! read the answer out of when the clever tier disagrees with it.
//!
//! # What pull based means here and what it does not mean
//!
//! [`Operator::next`] returns the next [`Chunk`](rudb_vector::Chunk) or `None` when there are no
//! more. A pipeline of a scan, a filter and a projection is three of those calls deep and nothing
//! is materialized between them. An operator that cannot answer without seeing all of its input,
//! which is the aggregate, the sort, the join build side, the distinct and the set operations, does
//! all of its work on the first call to `next` and then hands out what it built one chunk at a
//! time. That is what `spec/07-execution.md` calls a pipeline breaker and it is the boundary the
//! morsel driven scheduler will later cut pipelines at.
//!
//! What this is not is the scheduler. There is one thread, the morsels a source hands out are all
//! read by it, and the hash join is a nested loop. Every one of those is M1 or later work and every
//! one of them replaces an operator here without changing the tree that builds it, because the
//! thing that builds the tree is [`build`] and the thing it builds against is a trait with two
//! methods.
//!
//! # The move to push
//!
//! The interface every operator ends up behind is in `rudb-pipeline`, and they moved to it one at a
//! time rather than in one commit. Every one of them is there now. The table scan, the dummy scan,
//! the series, the file scan, the values list and the strategies table are
//! [`Source`](rudb_pipeline::Source) implementations, the filter, the projection, the limit and the
//! cross product are [`Stream`](rudb_pipeline::Stream) implementations, and the sort, the top N, the
//! distinct, the set operations, the aggregate and the join are [`Sink`](rudb_pipeline::Sink)
//! implementations. All of them take `&self` and are handed the mutable part separately, so one of
//! them can be instantiated on as many threads as F4 wants without copying its predicate or its key
//! list.
//!
//! A source is the one of the three that is shared rather than instanced, so the position it is up
//! to is an atomic and a morsel goes to whoever asks for it first. What a morsel covers is each
//! source's own business: one stored chunk for a table scan, a run of sixteen chunks for a series
//! because those rows are worked out rather than read, and the whole file list for a file scan,
//! since both file readers are a position in a file and cannot be asked for the tenth chunk without
//! having read the nine before it.
//!
//! What is left of the pull side is the shape of the tree and the adapters that drive it, which is
//! `adapt` and nothing else.
//!
//! Being in the shape is not the same as being parallel. The aggregate holds its hash table in the
//! instance, which is where it has to be, and merging two of those tables needs a serialize and a
//! combine per aggregate that nothing implements yet, so a second instance is refused rather than
//! answered wrongly. That is the one place where F4 has work left in an operator rather than in the
//! scheduler.
//!
//! An operator with two inputs is two pipelines with an edge between them, and the set operation
//! and the join are both built that way. The side that has to finish first ends in a
//! `gather::Gather`, which holds its rows and does nothing else, and the side that uses it reads
//! them through a handle. That edge is the one the scheduler will read off the plan, and for the
//! join it is where the hash table goes when #62 replaces the nested loop.
//!
//! The cross product sits on that edge too, and it is the operator that made `rudb-pipeline` grow a
//! [`Progress::Again`](rudb_pipeline::Progress::Again). One of its input chunks becomes as many
//! output chunks as its right side has, which a stream could not say and a sink could only answer by
//! holding the whole product. Its right side is kept as chunks rather than rows, by the other sink
//! in `gather`, because it replays them as they stand.
//!
//! A sink finalises into a `buffer::Buffered`, which is a separate source that reads the finished
//! chunks back out, rather than handing them back from `finalize`. That split is what makes the
//! parallel read possible later and it costs nothing now.
//!
//! `adapt` is the one thing that knows how to put a pushing operator in a pulling tree, and now
//! that every operator has moved it is the whole of the pull side. What it does not do yet is cut
//! the tree into pipelines and hand them to [`run_serial`](rudb_pipeline::run_serial), which is the
//! next step and the one that deletes this file rather than changing it.
//!
//! # Why a schema per operator
//!
//! A bound plan refers to columns by [`ColumnBinding`](rudb_plan::ColumnBinding), which is a table
//! index and a position, and a chunk is a row of vectors with no names on it. Something has to turn
//! one into the other, and that something is [`Schema`]: it is what an operator says it produces,
//! it carries the binding alongside the name and the type, and [`Schema::position_of`] is the whole
//! of expression column resolution. Building it is where the operators agree with the binder about
//! what a table index means, and it is checked rather than assumed, because a schema that is one
//! column out produces a wrong answer instead of an error.

#![forbid(unsafe_code)]

mod adapt;
mod buffer;
mod build;
mod cancel;
mod expr;
mod gather;
mod group;
mod join;
mod key;
mod operator;
mod ordering;
mod prepared;
mod register;
mod rows;
mod schema;
mod setop;
mod sort;
mod source;
mod spill;
mod strategies;
mod stream;
mod table;
mod topn;
mod written;

#[cfg(test)]
mod tests;

pub use build::{build, build_measured, build_with};
pub use expr::{evaluate, evaluate_all};
pub use operator::Operator;
pub use prepared::{Prepared, Scratch};
pub use register::registries;
pub use schema::Schema;
pub use written::written;
