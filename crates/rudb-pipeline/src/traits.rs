//! The three operator traits, and the reasons they look like this.

use std::fmt;

use rudb_common::Result;
use rudb_vector::Chunk;

use crate::morsel::Morsel;
use crate::pool::Lease;
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

    /// How many morsels there are in total, when the source knows before it starts.
    ///
    /// This is how a small query stays on one thread. A scheduler that spins up thirty two
    /// instances to read one chunk has spent more on starting them than the query was ever going to
    /// cost, and the per query floor is one of the four axes the project is measured on. A source
    /// that already knows how much work it has is the honest place to ask, because there is no
    /// estimate involved: a table scan has a chunk count, a Parquet file has row groups, and a
    /// finished operator's buffer has however many chunks it built.
    ///
    /// An answer that is a little wrong costs a little. Too high and an instance starts, asks for a
    /// morsel, is told there are none and stops. Too low and some of the work runs on fewer threads
    /// than it could have. So a source with several files may count the first one and assume the
    /// rest look like it rather than opening all of them to be sure.
    ///
    /// `None` means the source does not know, and the scheduler takes that as permission to use
    /// every thread it has, because a source that cannot count its work is not thereby small.
    ///
    /// `threads` is what the scheduler would lend if the answer came back large enough to want it.
    /// Most sources have a fixed amount of work and ignore it. A source that can choose how finely
    /// to cut its work needs it, because cutting finer than there are threads to run the pieces
    /// costs the cutting and buys nothing.
    fn morsels(&self, threads: usize) -> Option<usize> {
        let _ = threads;
        None
    }

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

    /// Whether more than one instance of this operator may run at once.
    ///
    /// Almost every stream says yes and means it, because a stream transforms the chunk it is given
    /// and its local state is scratch space. The one that says no is a `LIMIT`, whose local state
    /// is how much of the limit it has used up, and four instances each allowed ten rows is forty
    /// rows and a wrong answer rather than a slow one.
    ///
    /// An operator in a pipeline that says no puts the whole pipeline on one thread. That is the
    /// blunt version on purpose: the alternative is a shared counter and a scan that stops when
    /// somebody else's rows filled the limit, which is a correct `LIMIT` and also a `LIMIT` whose
    /// answer depends on which thread got there first.
    fn parallel(&self) -> bool {
        true
    }

    /// Do whatever this operator needs doing once, before any instance of it runs.
    ///
    /// A hash join is why this is here. Its table is built from a side another pipeline gathered,
    /// it is shared by every instance, and until this existed it was built by whichever instance
    /// asked for it first, behind a lock the other instances waited on. That is one thread doing
    /// the work while the other nine sleep, and on TPC-H q9 it was 46 percent of all worker thread
    /// time spent in `semaphore_wait_trap`. The work itself is perfectly parallel and the threads
    /// to do it on are the ones this pipeline has already leased, which is what the argument is.
    ///
    /// Called once per run of the pipeline, on the thread that is about to hand the instances out,
    /// so an implementation may assume it is alone and may borrow the lease. An operator with
    /// nothing to do before it starts leaves this as it is and pays a virtual call per pipeline.
    ///
    /// # Errors
    ///
    /// Whatever the operator reports. A failure here fails the pipeline before any of it has run.
    fn prepare(&self, threads: &Lease<'_>) -> Result<()> {
        let _ = threads;
        Ok(())
    }

    /// Whether this operator owes chunks once every instance has finished reading.
    ///
    /// Almost nothing does, and the cost of asking is one virtual call per operator per pipeline,
    /// which is why it is a question rather than a drain that turns out to be empty. A driver that
    /// is told no by every operator skips building the state a drain would need.
    fn drains(&self) -> bool {
        false
    }

    /// The chunks this operator owes, once every instance has finished reading.
    ///
    /// An outer join that gathered the side it keeps is why this is here. It pairs a driving row
    /// with what it matched as the row arrives, which is a stream, and it also owes a padded row
    /// for every gathered row nothing matched, which is not known until the last driving row has
    /// been through. Without somewhere to put that second half such a join has to be a sink, and a
    /// sink materialises the whole answer rather than the part of it nobody could have produced
    /// earlier. TPC-H q13's `LEFT JOIN` produces 1.5 million pairs and owes about fifty thousand
    /// padded rows, so the part that has to wait is three percent of the answer and the other
    /// ninety seven percent streams.
    ///
    /// Called once per run of the pipeline, after every instance has finished its morsels and
    /// before the sink is finalised, on one thread. `out` takes each chunk and pushes it through
    /// whatever sits below this operator and into the sink, and answers [`Progress::Done`] when
    /// nothing below wants any more, which an implementation should take as leave to stop.
    ///
    /// # Errors
    ///
    /// Whatever the operator reports, and whatever `out` reports from below it.
    fn drain(&self, out: &mut dyn FnMut(&mut Chunk) -> Result<Progress>) -> Result<()> {
        let _ = out;
        Ok(())
    }

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

    /// Whether more than one instance of this operator may run at once.
    ///
    /// A sink that says yes is promising two things. That [`Sink::combine`] puts two instances
    /// together into the same state one instance would have reached, which is what the method is
    /// for. And that the order of what it finally produces does not depend on which instance got
    /// which morsel, either because it decides the order itself or because nothing downstream can
    /// tell.
    ///
    /// The second promise is the one that is easy to break. A sink that keeps its input as it
    /// arrives, for an operator above it to replay in that order, produces a different answer at
    /// two threads than at one, and the difference is an order nobody asked for rather than an
    /// error. Those say no until there is a reason and a mechanism for them to say yes.
    fn parallel(&self) -> bool {
        true
    }

    /// How many threads this sink could use in [`Sink::finalize`], given the chance.
    ///
    /// A pipeline used to borrow as many threads as its source had morsels, and finishing the sink
    /// happens after every one of those has joined, on the same borrowed threads. So a scan that
    /// cut itself into sixteen morsels on a thirty two thread machine left a hash aggregate
    /// finishing a million groups on sixteen threads, and the other half of the machine parked,
    /// because nothing in the pipeline ever said that the finish is a separate piece of work from
    /// the scan and is not the same width.
    ///
    /// This is a sink saying so. The default is one, which is what almost everything means: it
    /// finishes on the thread that asked and the lease does not need to be any wider than the
    /// source made it. A sink that finishes in parallel answers with what it could use, and the
    /// pipeline borrows the larger of that and what the source wants.
    ///
    /// It is asked before a row has been read, so a sink that cannot know yet should answer with
    /// what it would want if the query turns out to be large. Borrowing a thread that is then not
    /// used costs the borrow and nothing else, because the threads are parked in the pool and are
    /// never woken unless there is a piece of work to hand them.
    fn finalize_degree(&self, ceiling: usize) -> usize {
        let _ = ceiling;
        1
    }

    /// Do whatever this operator needs doing once, before any instance of it runs.
    ///
    /// The same hook as [`Stream::prepare`] and it is there for the same reason. A join that
    /// gathered the side it produces ends its pipeline rather than sitting in the middle of one,
    /// so it is a sink, and its table is still shared by every instance and still wants building
    /// on the whole lease rather than by whichever instance asked for it first.
    ///
    /// Called once per run of the pipeline, on the thread that is about to hand the instances out,
    /// so an implementation may assume it is alone and may borrow the lease. An operator with
    /// nothing to do before it starts leaves this as it is and pays a virtual call per pipeline.
    ///
    /// # Errors
    ///
    /// Whatever the operator reports. A failure here fails the pipeline before any of it has run.
    fn prepare(&self, threads: &Lease<'_>) -> Result<()> {
        let _ = threads;
        Ok(())
    }

    /// Told which morsel the chunks that come next were read from.
    ///
    /// Called once per morsel, by the driver, on the instance that took it, before any
    /// [`Sink::sink`] carrying rows from it. A sink that does not care where a chunk came from
    /// ignores this, which is the default and is every sink in the tree but one.
    ///
    /// The one that cares is the root. A parallel driver hands morsels out to whichever thread is
    /// free, so the chunks come back in whatever order the threads finished, and a query that asked
    /// for the rows in the order the file holds them would get a different answer on every run.
    /// Knowing which morsel a chunk came from is what lets the root put them back. It is a method on
    /// the trait rather than something the root works out for itself because the driver is the only
    /// thing that knows which morsel an instance is on, and this is how it says so.
    ///
    /// A source whose morsels are meant to be put back in order has to number them in that order.
    /// Nothing checks that, and a source that numbers them some other way gets its own numbering
    /// back rather than a wrong answer.
    ///
    /// # Errors
    ///
    /// Whatever taking note of the morsel reports.
    fn at(&self, _morsel: &Morsel, _local: &mut Self::Local) -> Result<()> {
        Ok(())
    }

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
    fn finalize(&self, threads: &Lease<'_>) -> Result<()>;
}
