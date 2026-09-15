//! Operators, morsels, the scheduler, hash tables, sorting and spilling.
//!
//! Rank 12 in the layer rule. See `xtask/layers.toml` and `spec/18-package-layout.md`.
//!
//! This is tier 0 of `spec/08-codegen.md` section 8.1: one implementation of each logical operator
//! the binder can produce, every one of them written the simplest way that is correct. Tier 0 is
//! never removed and never optional, because it is the reference every faster tier is differentially
//! tested against, and a reference that is clever is a reference nobody can read the answer out of
//! when the clever tier disagrees with it.
//!
//! # A query is a list of pipelines
//!
//! [`build`] turns a plan into a [`Query`], which is the pipelines that run it in the order they
//! have to run plus the queue the rows come out of. A pipeline is a run of operators from a source
//! to a sink, and a plan is cut into several of them wherever an operator cannot produce anything
//! until it has consumed everything: the aggregate, the sort, the top n, the distinct, the set
//! operations and the side of a join that is gathered first. [`Query::run`] hands each of them to
//! [`run_serial`](rudb_pipeline::run_serial), and that is the whole of the execution path.
//!
//! It used to be a tree of operators with a `next` on each of them, driven by pulling the root, and
//! the breakers drained the tree below them on the first pull. The order was the same order, because
//! a breaker cannot answer until its input is finished either way. What changed is that the order is
//! now written down as a list rather than being whatever the call stack happened to do, which is the
//! thing a scheduler can be handed.
//!
//! # The three interfaces
//!
//! They live in `rudb-pipeline`. The table scan, the dummy scan, the series, the file scan, the
//! values list, the strategies table and the buffer a breaker finalises into are
//! [`Source`](rudb_pipeline::Source) implementations, the filter, the projection, the limit, the
//! fetch and the cross product are [`Stream`](rudb_pipeline::Stream) implementations, and the sort,
//! the top N, the distinct, the set operations, the aggregate and the join are
//! [`Sink`](rudb_pipeline::Sink) implementations. All of them take `&self` and are handed the mutable
//! part separately, so one of them can be instantiated on as many threads as the scheduler wants
//! without copying its predicate or its key list.
//!
//! A source is the one of the three that is shared rather than instanced, so the position it is up
//! to is an atomic and a morsel goes to whoever asks for it first. What a morsel covers is each
//! source's own business: one stored chunk for a table scan, a run of sixteen chunks for a series
//! because those rows are worked out rather than read, one row group for a Parquet scan, and a whole
//! file for a CSV one, because nothing in a CSV says where a row begins until every byte before it
//! has been parsed.
//!
//! Being in the shape is not the same as being parallel. There is still one instance of each
//! pipeline, because the driver is the serial one, and the pool and the scheduler that run several
//! are the next milestone. What the operators already do is merge: a grouped aggregate combines a
//! second instance's table into the one it is keeping, so the thing standing between here and
//! several threads is the driver rather than the operators.
//!
//! An operator with two inputs is two pipelines with an edge between them, and the set operation
//! and the join are both built that way. The side that has to finish first ends in a
//! `gather::Gather`, which holds its rows and does nothing else, and the side that uses it reads
//! them through a handle. That edge is the one [`Query::run`] takes its order from, and for the join
//! it is where the hash table goes when #62 replaces the nested loop.
//!
//! The cross product sits on that edge too, and it is the operator that made `rudb-pipeline` grow a
//! [`Progress::Again`](rudb_pipeline::Progress::Again). One of its input chunks becomes as many
//! output chunks as its right side has, which a stream could not say and a sink could only answer by
//! holding the whole product. Its right side is kept as chunks rather than rows, by the other sink
//! in `gather`, because it replays them as they stand. Unlike every other two input operator it does
//! not start a pipeline of its own, because the product is produced a chunk at a time and never
//! held, so it stays in the pipeline its left rows came from.
//!
//! A sink finalises into a `buffer::Buffered`, which is a separate source that reads the finished
//! chunks back out, rather than handing them back from `finalize`. That split is what makes the
//! parallel read possible later and it costs nothing now, and it is what a pipeline downstream of a
//! breaker sources from.
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

mod buffer;
mod build;
mod expr;
mod fetch;
mod functionnames;
mod gather;
mod group;
mod join;
mod key;
mod keywords;
mod metadata;
mod ordering;
mod prepared;
mod query;
mod register;
mod rows;
mod schema;
mod setop;
mod settingnames;
mod sort;
mod source;
mod spill;
mod strategies;
mod stream;
mod table;
mod topn;
mod typenames;
mod written;

#[cfg(test)]
mod tests;

pub use build::{build, build_measured, build_with};
pub use expr::{evaluate, evaluate_all};
pub use prepared::{Prepared, Scratch};
pub use query::Query;
pub use register::registries;
pub use schema::Schema;
pub use written::written;
