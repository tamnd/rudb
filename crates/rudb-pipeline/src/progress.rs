//! How far an operator got, and the closed set of reasons it could not get further.

use std::fmt;

/// What one call to an operator achieved.
///
/// `Done` means two different things at two different scopes and both are "I am finished with what
/// I was given". For [`Source::read`](crate::Source::read) it means this morsel is drained and the
/// driver should ask for another. For [`Stream::push`](crate::Stream::push) and
/// [`Sink::sink`](crate::Sink::sink) it means no more input is wanted at all, which is how a
/// `LIMIT` stops a scan rather than filtering the rows it did not need after reading them.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum Progress {
    /// Did work, call again.
    More,
    /// Did work, and there is more output in the input this operator already has.
    ///
    /// Only a [`Stream`](crate::Stream) says this, and only an operator whose one input chunk
    /// becomes several output chunks: a cross product, an unnest, a window that pads its frame. The
    /// alternative is holding the whole product, and a thousand rows against a thousand is a million
    /// rows nobody asked to have in memory at once.
    ///
    /// The driver hands the chunk it was given on, and then calls the same operator again. What is
    /// in the chunk on that second call is whatever the operators below left in it, so an operator
    /// that says this has to be holding everything it still needs and has to overwrite the chunk
    /// rather than read it.
    Again,
    /// Did work, and that is the end of this unit.
    Done,
    /// Could not proceed. The scheduler parks the task and runs another.
    Blocked(Blocked),
}

/// Why an operator could not proceed.
///
/// Four reasons and the set is closed. This is DuckDB's shape and it was chosen over a design with
/// open ended wait tokens for one reason: with a closed set the wait for graph is a finite graph
/// over four kinds of edge, so deadlock is enumerable. The scheduler asserts acyclicity at plan
/// time and again whenever every task is blocked, and a cycle becomes a bug report with the cycle
/// in it rather than a hang somebody has to attach a debugger to.
///
/// The more elegant alternative gives exact backpressure and costs the ability to find a deadlock
/// by reading a graph instead of reading every state machine in the engine. For a project whose
/// test strategy is running a large corpus under many combinations of strategies, enumerable beats
/// elegant.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum Blocked {
    /// A read was issued and has not arrived.
    Io(IoToken),
    /// A reservation was refused and the memory has not come back yet.
    Memory(MemoryToken),
    /// Another pipeline has to finish first, for example a join's build side.
    Dependency(PipelineId),
    /// Whatever consumes this pipeline's output has not taken it yet.
    Downstream(BufferId),
}

impl Blocked {
    /// Which of the four it is, without the token.
    ///
    /// The metrics document reports blocked time by reason, and the reason is the useful half. How
    /// long a query spent waiting for memory as against waiting for a disk is the first thing
    /// anybody wants to know about a slow query and no current output shows it.
    #[must_use]
    pub const fn reason(self) -> BlockedReason {
        match self {
            Blocked::Io(_) => BlockedReason::Io,
            Blocked::Memory(_) => BlockedReason::Memory,
            Blocked::Dependency(_) => BlockedReason::Dependency,
            Blocked::Downstream(_) => BlockedReason::Downstream,
        }
    }
}

impl fmt::Display for Blocked {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Blocked::Io(token) => write!(f, "waiting for read {}", token.0),
            Blocked::Memory(token) => write!(f, "waiting for memory reservation {}", token.0),
            Blocked::Dependency(pipeline) => write!(f, "waiting for pipeline {}", pipeline.0),
            Blocked::Downstream(buffer) => write!(f, "waiting for buffer {} to drain", buffer.0),
        }
    }
}

/// One of the four reasons, without the token that identifies which one.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
#[non_exhaustive]
pub enum BlockedReason {
    /// Waiting for a read.
    Io,
    /// Waiting for memory.
    Memory,
    /// Waiting for another pipeline.
    Dependency,
    /// Waiting for a consumer.
    Downstream,
}

impl BlockedReason {
    /// Every reason, which is what a metrics document has a column for.
    pub const ALL: &'static [BlockedReason] = &[
        BlockedReason::Io,
        BlockedReason::Memory,
        BlockedReason::Dependency,
        BlockedReason::Downstream,
    ];

    /// The word the metrics document and `EXPLAIN ANALYZE` print.
    #[must_use]
    pub const fn name(self) -> &'static str {
        match self {
            BlockedReason::Io => "io",
            BlockedReason::Memory => "memory",
            BlockedReason::Dependency => "dependency",
            BlockedReason::Downstream => "downstream",
        }
    }
}

impl fmt::Display for BlockedReason {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.name())
    }
}

/// Identifies an outstanding read, so that the scheduler knows what to wait on.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct IoToken(pub u64);

/// Identifies the reservation an operator is waiting for.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct MemoryToken(pub u64);

/// Identifies a pipeline within one query.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct PipelineId(pub u32);

impl fmt::Display for PipelineId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "pipeline {}", self.0)
    }
}

/// Identifies a buffer between two pipelines.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct BufferId(pub u32);
