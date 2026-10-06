//! Rows that go to the caller while the query runs.
//!
//! A query result is normally all in memory before the caller sees the first row. That is the
//! right shape for a program that embeds the database and reads the result as a value, and the
//! wrong one for a server that sends the rows on a socket: the first row waits for the last one,
//! and a result of a hundred million rows is a hundred million rows of memory. A caller that wants
//! the rows as they come gives a [`RowSink`] to [`streaming`], and the next large query of the
//! statement it names sends its rows there instead of into the result.

use std::cell::RefCell;

use rudb_common::{LogicalType, Origin, Result};
use rudb_vector::Chunk;

/// Takes the rows of a query while it runs.
pub trait RowSink {
    /// The columns of the result, before the first chunk and before the query runs. An error
    /// stops the query before it runs.
    ///
    /// # Errors
    ///
    /// Whatever the sink says, which becomes the error of the statement.
    fn start(
        &mut self,
        names: &[String],
        types: &[LogicalType],
        origins: &[Option<Origin>],
    ) -> Result<()>;

    /// One chunk of rows, every column flat. An error stops the query.
    ///
    /// # Errors
    ///
    /// Whatever the sink says, which becomes the error of the statement.
    fn rows(&mut self, chunk: Chunk) -> Result<()>;
}

/// The sink and the text of the statement whose rows it takes.
struct Waiting {
    sql: String,
    sink: Box<dyn RowSink>,
}

thread_local! {
    /// The sink that [`streaming`] set on this thread, until a query takes it or `streaming` ends.
    static SINK: RefCell<Option<Waiting>> = const { RefCell::new(None) };
}

/// Runs `run` with `sink` ready to take the rows of the query of the statement `sql`.
///
/// Only a query on this thread whose text is `sql` takes the sink, so a query that a statement runs
/// for itself, such as the one that fills a default, does not. Only a query that reads a table and
/// is expected to give many rows takes it, because a thread to run the query costs more than a
/// small result. A result whose rows went to the sink has no rows and says so with
/// [`QueryResult::streamed`](crate::QueryResult::streamed).
pub fn streaming<R>(sql: &str, sink: Box<dyn RowSink>, run: impl FnOnce() -> R) -> R {
    SINK.with(|slot| *slot.borrow_mut() = Some(Waiting { sql: sql.to_owned(), sink }));
    let done = run();
    SINK.with(|slot| slot.borrow_mut().take());
    done
}

/// The sink for the statement `sql`, if [`streaming`] set one for it on this thread.
pub(crate) fn take(sql: &str) -> Option<Box<dyn RowSink>> {
    SINK.with(|slot| {
        let mut slot = slot.borrow_mut();
        if slot.as_ref().is_some_and(|waiting| waiting.sql == sql) {
            return slot.take().map(|waiting| waiting.sink);
        }
        None
    })
}

/// Whether a sink waits for the statement `sql` on this thread, without taking it.
pub(crate) fn waits(sql: &str) -> bool {
    SINK.with(|slot| slot.borrow().as_ref().is_some_and(|waiting| waiting.sql == sql))
}
