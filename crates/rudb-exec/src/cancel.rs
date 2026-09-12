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
///
/// What this cannot see is work an operator does inside one call to `next`. Pulling from a child is
/// not that, because the child is wrapped too, so an operator that reads its whole input is checked
/// all the way through the reading. An operator that then loops over what it read is, and [`Join`]
/// is the one that does: the nested loop runs to the end inside the first `next`, and a hundred
/// thousand left rows against thirty thousand right ones is a minute with nothing looking at the
/// token. So that loop holds the token as well and checks it once per left row. The rule is still
/// the one above, with a sentence after it: every operator is checked between its chunks, and an
/// operator whose own loop can outlive a chunk checks inside it.
///
/// [`Join`]: crate::join::Join
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
