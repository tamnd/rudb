//! What a rudb file knows about its own columns: the column summary and the sketches.
//!
//! This crate holds the payloads of `spec/stats/03-the-file-format.md` sections 3.3 and 3.4 and
//! nothing else. It knows what a [`Summary`] says, how two of them fold together, and what bytes
//! either of them is on disk. It deliberately does not know what a file, a page or a plan is, which
//! is the same split `rudb-graph` takes and for the same reason: `rudb-native` at rank 6 is what
//! puts a summary in a file and `rudb-opt` is what reads one to choose a plan, and a summary that
//! could see the format would be a summary that could only be tested through one.
//!
//! # The invariant, which is the same one
//!
//! Section 3.1: delete every statistics section from a rudb file and no query changes its answer.
//! Some get slower, none gets wrong. That is `../graph/03-the-file-format.md` section 3.1 applied to
//! a second kind of payload, and it is what makes a stale summary a performance bug rather than a
//! wrong answer.
//!
//! It has teeth here in a way it does not in the graph layer, because these sections do answer
//! queries. A `COUNT(DISTINCT c)` can come out of a summary without touching the column. So every
//! number in [`Summary`] carries a [`Class`] saying how much of it is knowledge, and the difference
//! between [`Class::Exact`] and [`Class::Estimated`] is the difference between answering a query
//! from metadata and choosing between two plans that return the same rows. An engine that confuses
//! the two returns a wrong answer very fast, which is the worst outcome available.
//!
//! # Mergeable or re-derivable, with nothing in between
//!
//! Every fact stored here is one of two things: something that folds when two parts are folded, or
//! something a single pass over one part can compute again. Nothing is stored that needs the whole
//! table to have been seen at once, because a table gets appended to and a statistic that cannot
//! survive an append is a statistic that is wrong by the second week.
//!
//! Counts add. Extremes take the smaller and the larger. Byte totals add and widths take the
//! larger. Sketches union exactly, which is the whole reason a KMV sketch is the structure here and
//! a HyperLogLog is not.
//!
//! The two that do not fold are worth naming, because the answer to both is to weaken rather than to
//! guess. A distinct count does not add: the union of two parts holding a thousand values each
//! holds between a thousand and two thousand. So [`Summary::widen`] takes the larger of the two and
//! marks it a lower bound, which is true and useful and cheap, and the merged sketch is what turns
//! it back into a number. Distinctness does not survive a fold either, because two parts can each
//! hold no repeats and still repeat each other, so it survives only when the two ranges are
//! provably apart and is dropped otherwise.

pub mod sketches;
pub mod summary;

pub use rudb_common::stat::{Class, Direction};
pub use sketches::{STRIPE_K, Sketches};
pub use summary::{Order, Summary};
