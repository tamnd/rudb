//! The list of seams.
//!
//! A seam is a point in the engine where two or more published designs disagree about how to do
//! the same thing. The list is closed and it lives here rather than being inferred from whichever
//! registries happen to have been built, because it is a public artifact: `rudb_strategies()`
//! returns it, `EXPLAIN` names from it, the sweep enumerates it, and a list that is discovered
//! rather than declared is a different list on a different platform.
//!
//! Most of these have no registry yet. That is deliberate and it is why [`SeamId::milestone`]
//! exists: the table is the plan as much as it is the interface, and a seam that is named with
//! nothing behind it says which milestone owes it.

use std::fmt;

/// One place where implementations can be swapped.
///
/// Adding a variant means adding a seam, which is a design decision and should show up in review
/// as one. Renaming one breaks a settings key that somebody has in a script, so the names are
/// stable in the same way an error code is stable.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
#[non_exhaustive]
pub enum SeamId {
    /// How a column carries its values.
    VectorForm,
    /// How two columns are compared into a mask.
    KernelCompare,
    /// How a mask becomes a selection.
    KernelFilter,
    /// How an expression tree becomes something that runs.
    ExprEval,
    /// When a sparse chunk is worth rebuilding densely.
    ChunkCompaction,
    /// How a string is represented in a column.
    StringRepr,
    /// How a block of values is encoded on the way to disk.
    StorageEncoder,
    /// What a dictionary is scoped to.
    StorageDictionary,
    /// Which page the buffer manager takes back first.
    BufferEviction,
    /// What happens when an operator is over its budget.
    SpillPolicy,
    /// How pipeline work is handed to threads.
    Scheduler,
    /// How much input one unit of that work covers.
    MorselSize,
    /// How a key becomes a hash.
    HashFunction,
    /// How a group of columns becomes one comparable key.
    HashKey,
    /// The hash table itself.
    HashTable,
    /// How an aggregate carries what it has seen so far.
    AggState,
    /// How partial aggregates from many threads are combined.
    AggParallel,
    /// How the top k rows are found.
    TopK,
    /// How the build side of a join is made probeable.
    JoinBuild,
    /// What the join pushes into the scan under its probe side.
    JoinFilter,
    /// When a column referenced by a query is actually read.
    ScanMaterialisation,
    /// How joins are ordered.
    OptJoinOrder,
    /// How many rows the optimizer thinks an operator produces.
    OptCardinality,
    /// How rows are sorted.
    Sort,
    /// How a window frame is evaluated.
    Window,
    /// How the choice at every other seam is made.
    Policy,
    /// How a chunk gets from one pipeline instance to another.
    ExchangeTransport,
}

impl SeamId {
    /// Every seam, in the order the design document lists them.
    ///
    /// The sweep, `rudb_strategies()` and the settings surface all walk this, so the order is the
    /// order a user sees and it is worth it being the order somebody chose.
    pub const ALL: &'static [SeamId] = &[
        SeamId::VectorForm,
        SeamId::KernelCompare,
        SeamId::KernelFilter,
        SeamId::ExprEval,
        SeamId::ChunkCompaction,
        SeamId::StringRepr,
        SeamId::StorageEncoder,
        SeamId::StorageDictionary,
        SeamId::BufferEviction,
        SeamId::SpillPolicy,
        SeamId::Scheduler,
        SeamId::MorselSize,
        SeamId::HashFunction,
        SeamId::HashKey,
        SeamId::HashTable,
        SeamId::AggState,
        SeamId::AggParallel,
        SeamId::TopK,
        SeamId::JoinBuild,
        SeamId::JoinFilter,
        SeamId::ScanMaterialisation,
        SeamId::OptJoinOrder,
        SeamId::OptCardinality,
        SeamId::Sort,
        SeamId::Window,
        SeamId::Policy,
        SeamId::ExchangeTransport,
    ];

    /// The stable name, which is what a setting, a hint and a sweep argument all spell.
    #[must_use]
    pub const fn name(self) -> &'static str {
        match self {
            SeamId::VectorForm => "vector.form",
            SeamId::KernelCompare => "kernel.compare",
            SeamId::KernelFilter => "kernel.filter",
            SeamId::ExprEval => "expr.eval",
            SeamId::ChunkCompaction => "chunk.compaction",
            SeamId::StringRepr => "string.repr",
            SeamId::StorageEncoder => "storage.encoder",
            SeamId::StorageDictionary => "storage.dictionary",
            SeamId::BufferEviction => "buffer.eviction",
            SeamId::SpillPolicy => "spill.policy",
            SeamId::Scheduler => "scheduler",
            SeamId::MorselSize => "morsel.size",
            SeamId::HashFunction => "hash.function",
            SeamId::HashKey => "hash.key",
            SeamId::HashTable => "hash.table",
            SeamId::AggState => "agg.state",
            SeamId::AggParallel => "agg.parallel",
            SeamId::TopK => "topk",
            SeamId::JoinBuild => "join.build",
            SeamId::JoinFilter => "join.filter",
            SeamId::ScanMaterialisation => "scan.materialisation",
            SeamId::OptJoinOrder => "opt.join-order",
            SeamId::OptCardinality => "opt.cardinality",
            SeamId::Sort => "sort",
            SeamId::Window => "window",
            SeamId::Policy => "policy",
            SeamId::ExchangeTransport => "exchange.transport",
        }
    }

    /// One line saying what the seam is a choice about.
    #[must_use]
    pub const fn describe(self) -> &'static str {
        match self {
            SeamId::VectorForm => "how a column carries its values",
            SeamId::KernelCompare => "comparing two columns into a mask",
            SeamId::KernelFilter => "turning a mask into a selection",
            SeamId::ExprEval => "turning an expression tree into something that runs",
            SeamId::ChunkCompaction => "when a sparse chunk is worth rebuilding densely",
            SeamId::StringRepr => "how a string is represented in a column",
            SeamId::StorageEncoder => "how a block of values is encoded on the way to disk",
            SeamId::StorageDictionary => "what a dictionary is scoped to",
            SeamId::BufferEviction => "which page the buffer manager takes back first",
            SeamId::SpillPolicy => "what happens when an operator is over its budget",
            SeamId::Scheduler => "how pipeline work is handed to threads",
            SeamId::MorselSize => "how much input one unit of work covers",
            SeamId::HashFunction => "turning a key into a hash",
            SeamId::HashKey => "turning a group of columns into one comparable key",
            SeamId::HashTable => "the hash table itself",
            SeamId::AggState => "how an aggregate carries what it has seen so far",
            SeamId::AggParallel => "combining partial aggregates from many threads",
            SeamId::TopK => "finding the top k rows",
            SeamId::JoinBuild => "making the build side of a join probeable",
            SeamId::JoinFilter => "what the join pushes into the scan under its probe side",
            SeamId::ScanMaterialisation => "when a column a query references is actually read",
            SeamId::OptJoinOrder => "the order the joins run in",
            SeamId::OptCardinality => "how many rows the optimizer thinks an operator produces",
            SeamId::Sort => "sorting rows",
            SeamId::Window => "evaluating a window frame",
            SeamId::Policy => "making the choice at every other seam",
            SeamId::ExchangeTransport => "moving a chunk from one pipeline instance to another",
        }
    }

    /// The milestone that owes this seam its first two implementations.
    ///
    /// A seam whose milestone has not started has an empty registry, and `rudb_strategies()`
    /// prints it that way rather than hiding it, because the list of what is not built yet is
    /// worth as much to somebody reading it as the list of what is.
    #[must_use]
    pub const fn milestone(self) -> &'static str {
        match self {
            SeamId::VectorForm
            | SeamId::KernelCompare
            | SeamId::KernelFilter
            | SeamId::ExprEval
            | SeamId::ChunkCompaction
            | SeamId::StringRepr => "F1",
            SeamId::StorageEncoder | SeamId::StorageDictionary => "F2",
            SeamId::BufferEviction | SeamId::SpillPolicy => "F3",
            SeamId::Scheduler | SeamId::MorselSize => "F4",
            SeamId::HashFunction
            | SeamId::HashKey
            | SeamId::HashTable
            | SeamId::AggState
            | SeamId::AggParallel
            | SeamId::TopK => "F5",
            SeamId::JoinBuild | SeamId::JoinFilter => "F6",
            SeamId::ScanMaterialisation => "F7",
            SeamId::OptJoinOrder | SeamId::OptCardinality => "F8",
            SeamId::Sort | SeamId::Window => "F9",
            SeamId::Policy => "F10",
            SeamId::ExchangeTransport => "F11",
        }
    }

    /// The seam with this name, or `None` if nothing is called that.
    ///
    /// This is how a settings key, a sweep argument and a query hint all reach the same place.
    #[must_use]
    pub fn from_name(name: &str) -> Option<SeamId> {
        SeamId::ALL.iter().copied().find(|seam| seam.name() == name)
    }
}

impl fmt::Display for SeamId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.name())
    }
}
