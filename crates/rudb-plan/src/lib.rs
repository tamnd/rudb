//! The logical plan, its textual form, and the parser that reads that form back.
//!
//! Rank 9 in the layer rule. See `xtask/layers.toml` and `spec/18-package-layout.md`.
//!
//! This is the bound logical plan: what the binder produces, what the optimizer rewrites, and what
//! the physical planner consumes. `spec/04-architecture.md` calls it "a bound logical plan with
//! fully resolved types", and both halves of that are enforced here rather than assumed. Every
//! expression carries a [`LogicalType`](rudb_common::LogicalType) that is stored next to it, and
//! every column reference is a [`ColumnBinding`] naming the operator that produced the column and
//! the position within that operator's output. There are no names in an expression and nothing in
//! this crate looks anything up in a catalog. Name resolution happened in the binder and a plan
//! that still needs it is a plan that is not bound.
//!
//! # The textual form
//!
//! `spec/00-README.md` requires that every layer has a textual form and a round-trip parser, and
//! that requirement is the reason this crate exists before there is an optimizer to rewrite
//! anything. A plan prints as an indented tree, two spaces a level, parent before children:
//!
//! ```text
//! Project #2 [#1.0::VARCHAR AS SearchPhrase, #1.1::BIGINT AS c]
//!   Limit 10 offset 0
//!     Sort [#1.1::BIGINT DESC NULLS LAST]
//!       Aggregate #1 groups=[#0.0::VARCHAR] aggregates=[count_star()::BIGINT]
//!         Filter (#0.0::VARCHAR <> ''::VARCHAR)::BOOLEAN
//!           Get memory.main.hits AS hits #0 [SearchPhrase::VARCHAR]
//! ```
//!
//! [`Plan::parse`] reads that back, and printing the result produces the same text. That fixed
//! point is a test rather than a claim, and it is the thing that makes a plan diffable across a
//! rewrite, fuzzable on its own, and bisectable when a pass starts returning a wrong answer.
//!
//! Every expression is written `form::TYPE`. The annotation is on every node and not only on the
//! ones where a reader would need it, because the alternative is a parser that has to re-derive
//! types, and re-deriving types means consulting the function catalog, and a dump that cannot be
//! read without a catalog is not a dump. It is verbose. It is also exact, and exact is the whole
//! job here.
//!
//! # Why an arena
//!
//! Nodes, expressions and their lists all live in flat vectors and refer to each other by `u32`
//! index, the same shape [`rudb_parse::Ast`](https://docs.rs/rudb-parse) uses. A plan is rewritten
//! many times by `spec/09-optimizer.md`'s fixed pass sequence, and a rewrite of a boxed tree is a
//! traversal that allocates at every node. It also makes a plan one owned value that clones with
//! three memcpys, which is what lets a pass be a pure function from plan to plan without that
//! being expensive.
//!
//! The cost is that a reference is a number and a number can point at the wrong thing.
//! [`Plan::validate`] is the answer to that, and it is what section 9.1 means by the invariant
//! every pass has to preserve.
//!
//! # What is not here yet
//!
//! Window functions, subquery expressions, correlated references, `UNNEST`, lambdas, prepared
//! statement parameters, and everything on the write side. The M0 transformer cannot produce any
//! of them, so a representation for them here would be a representation nothing has ever
//! constructed, which is a representation that is wrong in a way nobody finds out about. Neither
//! [`Expr`] nor [`Node`] is `#[non_exhaustive]`, which is deliberate: adding a plan node should
//! stop the build in every optimizer pass that has to decide what to do about it.

#![forbid(unsafe_code)]

mod expr;
mod node;
mod parse;
mod plan;
mod print;

pub use expr::{Arm, ColumnBinding, CompareOp, ConjunctionOp, Expr, SortKey};
pub use node::{JoinKind, Node, SetOpKind};
pub use plan::Plan;

/// A reference to an expression in [`Plan`]'s expression arena.
pub type ExprRef = u32;

/// A reference to a node in [`Plan`]'s node arena.
pub type NodeRef = u32;

/// A reference to an interned string in [`Plan`]'s string table.
pub type StrRef = u32;

/// A reference to a constant in [`Plan`]'s value table.
pub type ValueRef = u32;

/// A contiguous run in one of [`Plan`]'s pools.
///
/// Which pool is decided by the field that holds the slice, the same way a `u32` reference is only
/// meaningful in the arena it came from. A slice is `Copy` and eight bytes, so a node holding four
/// of them is still a node that fits in a cache line.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Slice {
    /// Index of the first element.
    pub start: u32,
    /// Number of elements.
    pub len: u32,
}

impl Slice {
    /// The empty slice.
    pub const EMPTY: Self = Self { start: 0, len: 0 };

    /// Whether the run has no elements.
    #[must_use]
    pub fn is_empty(self) -> bool {
        self.len == 0
    }

    /// The run as a range, for indexing the pool it belongs to.
    #[must_use]
    pub fn range(self) -> std::ops::Range<usize> {
        self.start as usize..(self.start as usize + self.len as usize)
    }
}
