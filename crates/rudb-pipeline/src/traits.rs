//! The three operator traits, and the reasons they look like this.

use std::fmt;

use rudb_common::Result;
use rudb_vector::Chunk;

use crate::morsel::Morsel;
use crate::progress::Progress;

/// Produces chunks. One instance, many concurrent readers.
///
/// A source is shared. Every thread running an instance of the pipeline calls the same object, so
/// [`Source::morsel`] has to be internally synchronised and has to be cheap, because it is called
/// once per unit of work by every thread and a lock held across a read would serialise the scan.
pub trait Source: Send + Sync + fmt::Debug {
    /// A unit of work, or `None` when there is no more.
    ///
    /// Called concurrently from every thread running this pipeline.
    fn morsel(&self) -> Option<Morsel>;

    /// Fill `out` from `morsel`, overwriting whatever was in it.
    ///
    /// May be called many times for one morsel, returning [`Progress::More`] until the morsel is
    /// drained and [`Progress::Done`] on the call that drains it. That is what lets a morsel be
    /// larger than a chunk, which the storage format needs, because a block of a wide string
    /// column does not fit in a chunk.
    ///
    /// The contract is overwrite and not append. A source that adds to what was already in `out`
    /// will produce a chunk that grows every call and a driver has no way to notice.
    ///
    /// # Errors
    ///
    /// Whatever reading, decoding or casting reports, carrying the message the user sees.
    fn read(&self, morsel: &mut Morsel, out: &mut Chunk) -> Result<Progress>;
}

/// Transforms chunks in place. No state that outlives one pipeline instance.
///
/// The in place part is the whole point. A filter writes a selection, a projection replaces
/// columns, and neither allocates a new chunk, which is what lets a chain of streaming operators
/// run in one cache resident buffer. An implementation that builds a new chunk and assigns it is a
/// pull engine wearing a push costume, and at F0 several of them are exactly that, because the
/// vector layer cannot be written into yet. F1 is where that stops being true, and the interface
/// is this shape now so that F1 changes implementations rather than signatures.
pub trait Stream: Send + Sync + fmt::Debug {
    /// Everything this operator mutates while it runs.
    ///
    /// One per pipeline instance, never shared. If an operator has no such state this is `()` and
    /// the compiler charges nothing for it.
    type Local: Send + 'static;

    /// Fresh local state for one instance.
    fn local(&self) -> Self::Local;

    /// Transform `chunk` in place.
    ///
    /// May shrink it through its selection and may replace its columns. May not grow it past the
    /// chunk width, because the buffer downstream operators write into was sized once.
    ///
    /// [`Progress::Done`] here means this operator wants no more input at all, which is how a
    /// `LIMIT` stops a scan rather than reading rows in order to discard them. The chunk it
    /// returns `Done` with is still delivered.
    ///
    /// [`Progress::Again`] means the opposite end of the same idea: there is more output in the
    /// input this operator was already given, so it should be called again once the chunk it just
    /// produced has been through everything below it. That is what a cross product needs, and it is
    /// the reason it is not written as a sink that holds the whole product. An operator that says so
    /// keeps whatever it still needs itself, because the chunk it is handed on the next call holds
    /// whatever the operators below it left in it.
    ///
    /// # Errors
    ///
    /// Whatever an expression, a cast or a kernel reports.
    fn push(&self, chunk: &mut Chunk, local: &mut Self::Local) -> Result<Progress>;
}

/// Consumes chunks into state. This is where a pipeline ends.
pub trait Sink: Send + Sync + fmt::Debug {
    /// Everything one instance accumulates before it is merged.
    type Local: Send + 'static;

    /// Fresh local state for one instance.
    fn local(&self) -> Self::Local;

    /// Take one chunk into the local state.
    ///
    /// [`Progress::Done`] means no more input is wanted, with the same meaning as on
    /// [`Stream::push`].
    ///
    /// # Errors
    ///
    /// Whatever accumulating the chunk reports, including running out of memory, which is an error
    /// and never an abort.
    fn sink(&self, chunk: &Chunk, local: &mut Self::Local) -> Result<Progress>;

    /// Merge one instance's local state into the global state.
    ///
    /// Takes the local state by value, which is not a detail. Consuming it is what makes merging
    /// one thread's state twice impossible, and merging it twice is the kind of bug that produces
    /// a wrong `SUM` once in every few hundred runs and is never reproduced by the person who
    /// reported it.
    ///
    /// # Errors
    ///
    /// Whatever merging reports.
    fn combine(&self, local: Self::Local) -> Result<()>;

    /// Called once, after every [`Sink::combine`].
    ///
    /// Produces whatever the next pipeline sources from. It is separate from that next source on
    /// purpose: a hash aggregate finalises into a structure and a separate source reads that
    /// structure out in parallel, and fusing the two would make the parallel read impossible.
    ///
    /// # Errors
    ///
    /// Whatever finishing the state reports.
    fn finalize(&self) -> Result<()>;
}
