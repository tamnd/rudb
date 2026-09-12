//! What every implementation behind a seam has in common.

use std::fmt;

use crate::context::Context;

/// One named implementation of one seam.
///
/// This trait carries nothing about what the implementation does. The seam's own trait carries
/// that, and it names this one as a supertrait, so a hash table is a `HashTable` and a `Strategy`
/// and the registry only ever needs the second half.
///
/// Everything here is answerable without running anything, because all of it is read at plan time:
/// [`Strategy::applicable`] decides whether an implementation is a candidate at all, and the other
/// four end up in `EXPLAIN`, in `rudb_strategies()` and in the sweep's output.
pub trait Strategy: Send + Sync + fmt::Debug {
    /// The stable name, kebab case, part of the settings surface forever.
    ///
    /// Somebody will put this in a script and somebody else will put it in a paper, so renaming
    /// one is a breaking change in the same way renaming an error code is.
    fn name(&self) -> &'static str;

    /// One line, shown by `rudb_strategies()`.
    fn describe(&self) -> &'static str;

    /// Where the implementation came from, so that `EXPLAIN` can cite it.
    fn provenance(&self) -> Provenance;

    /// Whether this implementation can handle the situation the planner is in.
    ///
    /// A strategy that says no is skipped and never sees the data. The reference implementation of
    /// every seam returns `true` unconditionally, which is what makes it the thing the policy can
    /// always fall back to and the thing the oracle can always compare against.
    ///
    /// This is a required method on purpose. The answer is short, it is usually `true`, and making
    /// somebody write it is how the question gets asked at all.
    fn applicable(&self, context: &Context<'_>) -> bool;

    /// Whether this implementation gives the same answer twice.
    ///
    /// The differential test reads this. A strategy that declares [`Determinism::Exact`] is
    /// compared bit for bit against the reference, which is what we want almost everywhere, and a
    /// strategy that sums floats in an order that depends on the thread count declares
    /// [`Determinism::PerThreadCount`] and is compared within a tolerance. Declaring it is what
    /// lets the comparison stay strict for everything that has not declared otherwise, and a
    /// blanket float tolerance across the whole suite hides real bugs.
    fn deterministic(&self) -> Determinism {
        Determinism::Exact
    }
}

/// Where an implementation came from.
///
/// The first question anybody asks about a number is what produced it, and the second is where
/// that came from. `EXPLAIN` printing `hash.table = unchained (Unchained..., DaMoN 2024)` answers
/// both without anybody opening a file.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum Provenance {
    /// The obviously correct one, kept forever, never the fast one.
    Reference,
    /// Somebody published it.
    Paper {
        /// The paper's title, short form if the full one runs long.
        title: &'static str,
        /// Where it appeared, for example `DaMoN` or `SIGMOD`.
        venue: &'static str,
        /// The year it appeared.
        year: u16,
    },
    /// Ours, with nothing published behind it.
    Ours,
}

impl fmt::Display for Provenance {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Provenance::Reference => f.write_str("reference"),
            Provenance::Paper { title, venue, year } => write!(f, "{title}, {venue} {year}"),
            Provenance::Ours => f.write_str("ours"),
        }
    }
}

/// How repeatable an implementation's answer is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum Determinism {
    /// The same input gives the same bytes out, every time, on every machine.
    Exact,
    /// The same input gives the same bytes out for a fixed thread count.
    ///
    /// Floating point addition is not associative, so a partial sum merged in a different order is
    /// a different sum. This is the honest answer for a parallel aggregate and pretending
    /// otherwise would make the differential test fail on a machine with a different core count.
    PerThreadCount,
    /// Not repeatable at all.
    ///
    /// Nothing in the tree declares this yet and nothing should without a reason written next to
    /// it, because a result that cannot be reproduced cannot be investigated.
    None,
}

impl fmt::Display for Determinism {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Determinism::Exact => f.write_str("exact"),
            Determinism::PerThreadCount => f.write_str("per thread count"),
            Determinism::None => f.write_str("none"),
        }
    }
}
