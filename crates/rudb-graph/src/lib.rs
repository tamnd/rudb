//! The graph layer's structures: row ids, key maps and relationships.
//!
//! This crate holds the data model of spec/graph/02-the-data-model.md and nothing else. It knows
//! what a [`Rid`] is, how a [`KeyMap`] turns a parent key value into one, and what a
//! [`Relationship`] declares. It deliberately does not know what a file, a page or a plan is:
//! `rudb-native` at rank 6 is what puts a key map on disk and `rudb-opt` is what decides to use
//! one, which is the right way round. A key map that could see the format would be a key map that
//! could only be tested through one.
//!
//! # The invariant everything here is built on
//!
//! Section 3.1: deleting every graph section from a rudb file must change no answer, only the time.
//! That one sentence is what makes this layer safe to build incrementally, and it has four
//! consequences worth stating where the code is rather than only in the specification.
//!
//! A wrong index is a performance bug and not a wrong answer. Nothing in this crate is ever the
//! only path to a row: a link join is a faster way to compute what a hash join computes, so a link
//! that resolves the wrong `rid` shows up as a differential test failure against the same query
//! with the graph sections turned off, which is a test that can exist because of the invariant.
//!
//! Staleness is handled by ignoring. A section carries a generation stamp, and a section whose
//! stamp does not match the table's is dropped rather than repaired. There is no repair path in
//! this crate, no incremental maintenance, and no way for a stale structure to produce an answer.
//!
//! Every query has a reference execution, reachable by flipping a setting. That is the whole
//! testing strategy for the milestones above G1.
//!
//! The cost of all three is the rule that pays for them: no section may hold information that is
//! not derivable from the table's own columns. A key map is a restatement of a key column. A
//! forward link is a restatement of a foreign key column. Neither adds a fact, which is exactly why
//! neither can be missed when it is gone.

pub mod bits;
pub mod keymap;
pub mod link;
pub mod rel;
pub mod rid;
pub mod wire;

pub use bits::BitVector;
pub use keymap::{DENSE_THRESHOLD, Form, KeyMap, Keys, Observed};
pub use link::{Bounds, Link};
pub use rel::{Cardinality, Relationship, Side, parse_links};
pub use rid::{NO_PARENT, PART_ROWS, Place, Places, Rid, STRIPE_PARTS};
pub use wire::Payload;
