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
///
/// # The lifetime
///
/// An operator may borrow from the plan and the catalog the query was built against. A scan reads
/// its rows out of the catalog's table rather than copying them, and an expression reads its
/// constants and its types out of the plan's arena, so the pipeline lives as long as the plan does
/// and no longer. That is what `'a` is, and it is why the trait objects carry it rather than being
/// the `'static` they default to. A pipeline built from operators that own everything they touch
/// is a `Pipeline<'static>` and nobody has to say so.
#[derive(Debug, Clone)]
pub struct Pipeline<'a> {
    id: PipelineId,
    source: Arc<dyn Source + 'a>,
    streams: Vec<Arc<dyn DynStream + 'a>>,
    sink: Arc<dyn DynSink + 'a>,
    depends_on: Vec<PipelineId>,
}

impl<'a> Pipeline<'a> {
    /// A pipeline with no streaming operators between the source and the sink.
    #[must_use]
    pub fn new(id: PipelineId, source: Arc<dyn Source + 'a>, sink: Arc<dyn DynSink + 'a>) -> Self {
        Self { id, source, streams: Vec::new(), sink, depends_on: Vec::new() }
    }

    /// Append a streaming operator. They run in the order they were added.
    #[must_use]
    pub fn then(mut self, stream: Arc<dyn DynStream + 'a>) -> Self {
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
    pub fn streams(&self) -> &[Arc<dyn DynStream + 'a>] {
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

    /// Whether every operator in this pipeline will run as more than one instance.
    ///
    /// One operator that says no is enough, because the instances of a pipeline are instances of
    /// all of it. There is no arrangement where a `LIMIT` runs once and the scan under it runs
    /// sixteen times, since the rows have to go through the limit on the thread that read them.
    #[must_use]
    pub fn parallel(&self) -> bool {
        self.streams.iter().all(|stream| stream.parallel()) && self.sink.parallel()
    }

    /// How many instances of this pipeline to run, given the most that may run at once.
    ///
    /// Three things bound it and the smallest wins. The ceiling, which is what the pool will lend.
    /// Whether the operators can be instanced at all. And how much work the source says it has,
    /// because an instance with no morsel to read is a thread that was started, told there is
    /// nothing for it and stopped, and on a query that was going to take two milliseconds that is
    /// the whole query.
    ///
    /// That last bound is the floor F4 asks for, arrived at by counting rather than by estimating.
    /// A scan of one stored chunk is one morsel and stays on one thread whatever the machine has,
    /// and a Parquet file of nine row groups uses nine threads and not the sixteen it was offered.
    /// The source is asked however few workers are coming for it, including when that is one. What
    /// it answers is a count, but asking is also how it is told what is about to happen, and a
    /// source that cuts its work differently for one worker than for eight cannot do that if the
    /// one worker case never reaches it. A scan of a stored table cuts a morsel per stripe rather
    /// than a morsel per part, and a morsel per part is one a scan cannot walk ruled out parts
    /// inside of, so a single threaded selective query used to pay for every part it had already
    /// proved held nothing.
    #[must_use]
    pub fn degree(&self, ceiling: usize) -> usize {
        let ceiling = if self.parallel() { ceiling.max(1) } else { 1 };
        self.source.gather(self.sink.gather_rows());
        match self.source.morsels(ceiling, self.weight()) {
            Some(work) => work.clamp(1, ceiling),
            None => ceiling,
        }
    }

    /// What one row costs this pipeline, counted in what the source spends reading it.
    ///
    /// One for the source, plus whatever each operator says it spends beyond an ordinary one. See
    /// [`Stream::weight`](crate::Stream::weight). The source is the only caller, and it is the
    /// caller because the source is what decides how many instances to run and the row count it
    /// used to decide on was only ever standing in for the work behind those rows.
    ///
    /// The source is asked as well as counted, because a source that took an operator's work off it
    /// is doing work this would otherwise have counted twice over or not at all. See
    /// [`Source::weight`](crate::Source::weight).
    #[must_use]
    pub fn weight(&self) -> usize {
        let streams = self.streams.iter().map(|stream| stream.row_weight()).sum::<usize>();
        1_usize
            .saturating_add(self.source.weight())
            .saturating_add(streams)
            .saturating_add(self.sink.row_weight())
    }

    /// How many threads to borrow for this pipeline, which is not always how many instances to run.
    ///
    /// The instances are what [`Pipeline::degree`] says, and that is decided by the source. What
    /// happens after them is the sink's finish, and it runs on the same borrowed threads with every
    /// instance already joined, so it is bounded by a number that was chosen for the scan. On a
    /// thirty two thread machine a million row scan cuts sixteen morsels and a hash aggregate then
    /// merged a million groups on sixteen threads with the other half of the machine parked.
    ///
    /// So the borrow is the larger of the two and the instance count stays the smaller. A thread
    /// borrowed and not used is parked in the pool and is never woken, which is what makes asking
    /// for the wider of the two cheap enough to do on every pipeline.
    #[must_use]
    pub fn lease_degree(&self, ceiling: usize) -> usize {
        self.widths(ceiling).1
    }

    /// The instance count and the borrow width, from one question to the source.
    ///
    /// Both numbers at once because [`Pipeline::degree`] is not a getter. Asking it is how the source
    /// is told what is about to happen, so a scan of a stored table reads its zone maps in there,
    /// drops the parts they rule out and cuts its morsels out of what is left. Asking twice does all
    /// of that twice and keeps the first answer, since the morsels are set once, so the second pass
    /// over the statistics is read and thrown away.
    ///
    /// The caller wants both, because it borrows the wider and runs the narrower, and it had been
    /// getting them from two calls. On TPC-H 12 that was every one of lineitem's 733 parts tested
    /// against the filter twice before a row was read. It also made the ruled out parts count double,
    /// which is how this was found: a scan that could reach nothing reported 1466 parts pruned out of
    /// a table that has 733.
    #[must_use]
    pub fn widths(&self, ceiling: usize) -> (usize, usize) {
        let degree = self.degree(ceiling);
        let ceiling = ceiling.max(1);
        (degree, degree.max(self.sink.finalize_width(ceiling)).clamp(1, ceiling))
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
