//! The one place every seam registry in the process is assembled.
//!
//! Registration is an explicit call in an explicit file, not a linker trick and not a macro that
//! collects implementations behind the reader's back. The list is a public artifact: `EXPLAIN`
//! prints from it, `rudb_strategies()` reads it, and `rudb-bench sweep --seam` enumerates it. A
//! list that is assembled by magic is a list nobody can check, and the first question about a
//! benchmark number is what produced it.
//!
//! This file is where a researcher who has written an implementation adds their one line. The rest
//! of their work is one file in one crate behind one seam trait.
//!
//! # What is registered
//!
//! One seam of the twenty seven. `chunk.compaction` has three implementations in `rudb-pipeline`
//! and this is where they are put in front of the engine. The other twenty six are named in
//! `SeamId` with the milestone that owes them written on each, which
//! [`SeamId::milestone`](rudb_seam::SeamId) answers and `rudb_strategies()` prints, so that table
//! reads as a list of what is planned rather than as an empty one.
//!
//! A seam that is registered here is also a seam a query can pin, which means the typed registry
//! has to be reachable by the operator that chooses from it as well as by the erased list that
//! `EXPLAIN` prints. That is why each one gets a named accessor next to the line that adds it.

use std::sync::{Arc, OnceLock};

use rudb_pipeline::Compaction;
use rudb_seam::{Registries, Registry};

/// Every registry in the process, assembled the first time somebody asks.
///
/// Built once and immutable after. A registry that could gain an entry after a query has planned
/// against it is a registry that makes two runs of the same query incomparable, and comparing runs
/// is the entire point of having one.
///
/// Public because `EXPLAIN` prints the seam section out of it and `EXPLAIN` is rendered above this
/// crate, in `rudb`. The optimizer cannot reach it, being under this crate in the layer rule, so the
/// caller that has both hands it over.
pub fn registries() -> &'static Registries {
    static REGISTRIES: OnceLock<Registries> = OnceLock::new();
    REGISTRIES.get_or_init(assemble)
}

/// The chunk compaction seam, which is what a filter chooses from once per query.
///
/// The same object the erased list holds, so what `EXPLAIN` prints and what runs cannot drift.
pub(crate) fn compaction() -> &'static Arc<Registry<dyn Compaction>> {
    static COMPACTION: OnceLock<Arc<Registry<dyn Compaction>>> = OnceLock::new();
    COMPACTION.get_or_init(|| Arc::new(rudb_pipeline::compaction()))
}

/// One line per crate that owns implementations of a seam.
fn assemble() -> Registries {
    let mut registries = Registries::new();
    registries.add(compaction().clone());
    registries
}
