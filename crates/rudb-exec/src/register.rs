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
//! # Why it is empty
//!
//! Nothing is registered yet. Twenty seven seams are named in `SeamId` and none of them has two
//! implementations in the tree, because F0 is the skeleton and every seam's first two
//! implementations belong to a later milestone, which [`SeamId::milestone`](rudb_seam::SeamId) says
//! for each of them. `rudb_strategies()` prints those twenty seven rows with the implementation
//! columns null, which is the honest state of the project and is meant to be read as a list of what
//! is planned rather than as an empty table.

use std::sync::OnceLock;

use rudb_seam::Registries;

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

/// One line per crate that owns implementations of a seam.
fn assemble() -> Registries {
    // Each line below will read `registries.add(Arc::new(rudb_vector::register()))` or its
    // equivalent for the crate that owns the seam. The first of them arrives with F1, which owes
    // the vector form, compare, filter and expression evaluation seams their first two
    // implementations each.
    Registries::new()
}
