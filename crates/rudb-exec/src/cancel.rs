//! The check that lets a running query be stopped.

use rudb_common::{Cancel, Result};
use rudb_vector::Chunk;

use crate::operator::Operator;
use crate::schema::Schema;

/// An operator that asks whether the query should stop before it produces anything.
///
/// [`build`](crate::build) wraps every node in one of these, rather than putting the check in the
/// operators that can loop for a long time. Two reasons, and the second is the important one. It is
/// one line in one place instead of a decision per operator, and a decision per operator is a
/// decision somebody gets wrong when they add the twentieth one. And "every operator checks before
/// it produces a chunk" is a sentence that can be read and believed, where "these six check" is a
/// claim that has to be re-audited every time an operator is added or a loop is moved.
///
/// The cost is an atomic load and an indirect call per chunk per level of the tree. A chunk is 1024
/// rows and the cheapest thing any operator does to one is measured in microseconds, so this is
/// somewhere under a thousandth of that, and it buys a query that stops when it is asked to. That
/// is the trade `spec/engine/10-scheduler.md` section 10.9 asks for in as many words: every
/// operator checks a cancellation flag at chunk granularity, because once per thousand rows is
/// cheap and once per row is not.
#[derive(Debug)]
pub(crate) struct Guarded<'a> {
    inner: Box<dyn Operator + 'a>,
    cancel: Cancel,
}

impl<'a> Guarded<'a> {
    /// Wraps an operator so the query can be stopped between its chunks.
    pub(crate) fn new(inner: Box<dyn Operator + 'a>, cancel: Cancel) -> Self {
        Self { inner, cancel }
    }
}

impl Operator for Guarded<'_> {
    fn schema(&self) -> &Schema {
        self.inner.schema()
    }

    fn next(&mut self) -> Result<Option<Chunk>> {
        self.cancel.check()?;
        self.inner.next()
    }
}
