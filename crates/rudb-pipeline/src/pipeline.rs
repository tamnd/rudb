//! One source, zero or more streams, one sink.

use std::sync::Arc;

use crate::dynamic::{DynSink, DynStream, LocalState};
use crate::progress::PipelineId;
use crate::traits::Source;

/// A pipeline.
///
/// A query is a directed acyclic graph of these. The cuts are at pipeline breakers, which are the
/// operators that cannot emit anything until they have consumed everything: a hash join's build
/// side, a hash aggregate, a sort, a top n, a window, a distinct and the set operations. An edge
/// is a dependency, so a join's probe pipeline cannot start until its build pipeline's finalize
/// has returned.
///
/// The operators are behind `Arc` because a pipeline is instantiated once per thread and every
/// instance shares the operator objects while having its own local state. At F0 there is one
/// instance and the `Arc` costs nothing, and at F4 the count changes and nothing else does.
#[derive(Debug, Clone)]
pub struct Pipeline {
    id: PipelineId,
    source: Arc<dyn Source>,
    streams: Vec<Arc<dyn DynStream>>,
    sink: Arc<dyn DynSink>,
    depends_on: Vec<PipelineId>,
}

impl Pipeline {
    /// A pipeline with no streaming operators between the source and the sink.
    #[must_use]
    pub fn new(id: PipelineId, source: Arc<dyn Source>, sink: Arc<dyn DynSink>) -> Self {
        Self { id, source, streams: Vec::new(), sink, depends_on: Vec::new() }
    }

    /// Append a streaming operator. They run in the order they were added.
    #[must_use]
    pub fn then(mut self, stream: Arc<dyn DynStream>) -> Self {
        self.streams.push(stream);
        self
    }

    /// Record that this pipeline cannot start until another has finalised.
    #[must_use]
    pub fn after(mut self, other: PipelineId) -> Self {
        self.depends_on.push(other);
        self
    }

    /// Which pipeline this is, within one query.
    #[must_use]
    pub fn id(&self) -> PipelineId {
        self.id
    }

    /// The source.
    #[must_use]
    pub fn source(&self) -> &dyn Source {
        self.source.as_ref()
    }

    /// The streaming operators, in the order they run.
    #[must_use]
    pub fn streams(&self) -> &[Arc<dyn DynStream>] {
        &self.streams
    }

    /// The sink.
    #[must_use]
    pub fn sink(&self) -> &dyn DynSink {
        self.sink.as_ref()
    }

    /// The pipelines that have to finish first.
    #[must_use]
    pub fn depends_on(&self) -> &[PipelineId] {
        &self.depends_on
    }

    /// Fresh local state for one instance of this pipeline.
    #[must_use]
    pub fn locals(&self) -> Locals {
        Locals {
            streams: self.streams.iter().map(|stream| stream.local_state()).collect(),
            sink: self.sink.local_state(),
        }
    }
}

/// One instance's worth of local state, one entry per operator that has any.
#[derive(Debug)]
pub struct Locals {
    /// One per streaming operator, in the same order.
    pub streams: Vec<LocalState>,
    /// The sink's.
    pub sink: LocalState,
}
