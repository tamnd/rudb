//! The one interface every operator has.

use std::fmt;

use rudb_common::Result;
use rudb_vector::Chunk;

use crate::schema::Schema;

/// A source of chunks.
///
/// Two methods, and the reason there are only two is that everything else an operator could be
/// asked is either a property of its schema or a property of the plan it came from. An operator
/// that needed a third method to be driven would be an operator the scheduler has to know the shape
/// of, and section 7.2's morsel driven scheduler is supposed to know only that a pipeline has a
/// source, some streaming operators and a sink.
///
/// `next` returning `Some` with an empty chunk is allowed and means nothing more than that this
/// call produced no rows, which is what a filter that rejected a whole batch does. Only `None`
/// means the operator is finished. Calling `next` again after `None` returns `None` again for every
/// operator here, which is what makes a driver loop safe to write as a `while let`.
pub trait Operator: fmt::Debug {
    /// The columns this operator produces.
    fn schema(&self) -> &Schema;

    /// The next batch, or `None` when there are no more.
    ///
    /// # Errors
    ///
    /// Anything an expression, a cast or a kernel reports, carrying the message the user sees.
    fn next(&mut self) -> Result<Option<Chunk>>;
}
