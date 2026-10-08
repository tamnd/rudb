//! The rows of a large query, sent on the socket while the query runs (document 05 section 5.9).
//!
//! The session gives the engine a [`RowSink`] before a statement that can give rows. When the
//! query of the statement reads a table and is expected to give many rows, the engine runs it on
//! a thread of its own and hands each chunk to the sink on the session thread. The sink encodes
//! the chunk into `DataRow` messages and writes them to the socket each time the output passes
//! the flush threshold. A write that blocks holds the session thread, and the engine does not make
//! more than a few chunks ahead of it, so a client that reads slowly holds the query in place.
//!
//! The output that the session had before the statement moves into the sink for the run and
//! back after it, so the messages go out in the order that they were written. A query that the
//! engine does not stream leaves the sink untouched, and the session sends the rows of the
//! result as before.

use std::cell::RefCell;
use std::io::{self, Write};
use std::rc::Rc;
use std::sync::Arc;

use rudb::{Chunk, RowSink};
use rudb_common::{LogicalType, Origin};
use rudb_pgtypes::{RowEncoder, TypeError};
use rudb_pgwire::{Field, OutBuf};

use super::zone::{self, Zone};
use super::{
    FLUSH_AT, Failure, Format, Runner, Severity, column_type, field, leading_words, output,
    write_notice,
};
use crate::stream::Stream;

/// What the rows of the statement go out after.
pub(super) enum Flow {
    /// The simple flow: a `RowDescription`, and the rows in the text format.
    Simple,
    /// A portal of `Bind`: the rows in the result formats of `Bind`. A `Describe` of the portal
    /// already sent the columns, and the run must give the same ones.
    Portal { formats: Vec<i16>, described: Option<(Vec<LogicalType>, Vec<Option<Origin>>)> },
}

/// The state of a streamed statement, which the session and the sink in the engine share.
pub(super) struct Streamed {
    /// A second handle on the socket of the session.
    socket: Stream,
    /// The output of the session, which moves here while the statement runs.
    out: OutBuf,
    flow: Flow,
    format: Format,
    /// The time zone of the output, made when the first rows go out.
    zone: Option<Zone>,
    zone_name: String,
    least: Severity,
    pid: i32,
    /// The text of the message and the offset of the statement in it, for the position of a
    /// notice.
    text: Arc<str>,
    offset: usize,
    encoder: RowEncoder,
    /// The rows that went out.
    rows: u64,
    /// The engine gave the columns, so the rows of the statement go out here.
    started: bool,
    /// The error of the statement, when the sink stopped it: it takes the place of the error that
    /// the engine reports for the stop.
    failed: Option<Failure>,
    /// The socket failed, and the session ends.
    lost: Option<io::Error>,
}

/// The sink that the engine holds, on the state that the session holds.
struct Sink(Rc<RefCell<Streamed>>);

/// The end of a streamed statement, for the session.
pub(super) struct Ended {
    /// The rows went out here, with their number.
    pub(super) rows: Option<u64>,
    /// The error of the statement that the sink found, which replaces the error of the engine.
    pub(super) failed: Option<Failure>,
    /// The socket failed.
    pub(super) lost: Option<io::Error>,
}

impl Ended {
    /// The end of a statement that did not stream.
    pub(super) fn none() -> Ended {
        Ended { rows: None, failed: None, lost: None }
    }
}

/// Whether the rows of the statement `sql` can go to a sink: a query, and not a statement that
/// changes data and gives rows with `RETURNING`, whose rows PostgreSQL sends after all the
/// changes are done.
pub(super) fn streamable(sql: &str) -> bool {
    sql.trim_start().starts_with('(')
        || matches!(
            leading_words(sql).first().map(String::as_str),
            Some("SELECT" | "TABLE" | "VALUES")
        )
}

impl Runner {
    /// Runs `run` with a sink for the rows of the statement `sql`, when the session has a socket
    /// that a second handle can write: TLS keeps its state in one handle. The output of the
    /// session moves into the sink while `run` runs, and `out` holds all the output in order when
    /// this returns. The statement is at `offset` of `text`, the text of the message.
    pub(super) fn streamed<R>(
        &mut self,
        sql: &str,
        (text, offset): (&Arc<str>, usize),
        flow: Flow,
        out: &mut OutBuf,
        run: impl FnOnce(&mut Runner, &mut OutBuf) -> R,
    ) -> (R, Ended) {
        let Some(socket) = self.socket.take() else {
            return (run(self, out), Ended::none());
        };
        let state = Rc::new(RefCell::new(Streamed {
            socket,
            out: std::mem::take(out),
            flow,
            format: self.format,
            zone: None,
            zone_name: self.zone_name.clone(),
            least: self.least,
            pid: self.pid,
            text: Arc::clone(text),
            offset,
            encoder: RowEncoder::default(),
            rows: 0,
            started: false,
            failed: None,
            lost: None,
        }));
        let done = rudb::streaming(sql, Box::new(Sink(state.clone())), || run(self, out));
        let Ok(state) = Rc::try_unwrap(state).map(RefCell::into_inner) else {
            unreachable!("the engine keeps the sink of a statement only while the statement runs");
        };
        // What the session wrote while the statement ran comes after what the sink wrote.
        let during = std::mem::replace(out, state.out);
        out.bytes_mut().extend_from_slice(during.as_bytes());
        self.socket = Some(state.socket);
        let rows = state.started.then_some(state.rows);
        (done, Ended { rows, failed: state.failed, lost: state.lost })
    }
}

impl Streamed {
    /// Writes the output to the socket.
    fn flush(&mut self) -> rudb::Result<()> {
        let sent = self.socket.write_all(self.out.as_bytes()).and_then(|()| self.socket.flush());
        match sent {
            Ok(()) => {
                self.out.consume(self.out.len());
                Ok(())
            }
            Err(error) => {
                self.lost = Some(error);
                Err(rudb::Error::interrupt("the client connection was lost"))
            }
        }
    }

    /// Stops the statement with `failure` as its error.
    fn fail(&mut self, failure: Failure) -> rudb::Result<()> {
        let message = failure.message.clone();
        self.failed = Some(failure);
        Err(rudb::Error::interrupt(message))
    }

    /// Writes the notices that the statement raised before its rows, such as those of the parse.
    fn raised(&mut self) {
        // The sink runs on the session thread, which ran the parse.
        for notice in rudb_common::notice::take() {
            write_notice(&mut self.out, self.least, &notice, &self.text, self.offset);
        }
    }

    /// Writes the warnings of the advisory lock functions that the rows so far raised.
    fn warnings(&mut self) {
        if Severity::Warning < self.least {
            drop(rudb_common::advisory::warnings(self.pid));
            return;
        }
        for warning in rudb_common::advisory::warnings(self.pid) {
            self.out.notice_response(&[
                (b'S', Severity::Warning.word()),
                (b'V', Severity::Warning.word()),
                (b'C', b"01000"),
                (b'M', warning.as_bytes()),
            ]);
        }
    }
}

impl RowSink for Sink {
    fn start(
        &mut self,
        names: &[String],
        types: &[LogicalType],
        origins: &[Option<Origin>],
    ) -> rudb::Result<()> {
        let mut state = self.0.borrow_mut();
        state.started = true;
        // PostgreSQL parses the statement before it describes the rows.
        state.raised();
        let origin = |at: usize| origins.get(at).copied().flatten();
        let mut columns = Vec::with_capacity(types.len());
        let mut typmods = Vec::with_capacity(types.len());
        match &state.flow {
            Flow::Simple => {
                let fields: Vec<Field<'_>> = names
                    .iter()
                    .zip(types)
                    .enumerate()
                    .map(|(at, (name, ty))| field(name, ty, origin(at), 0))
                    .collect();
                state.out.row_description(&fields);
                for (at, logical) in types.iter().enumerate() {
                    let ty = column_type(logical, origin(at));
                    columns.push((logical.clone(), ty.oid, false));
                    typmods.push(ty.typmod);
                }
            }
            Flow::Portal { formats, described } => {
                if let Some((before, from)) = described {
                    let same = before == types
                        && (0..before.len())
                            .all(|at| from.get(at).copied().flatten() == origin(at));
                    if !same {
                        let message = "cached plan must not change result type".to_owned();
                        return state.fail(Failure::new("0A000", message));
                    }
                }
                if formats.len() > 1 && formats.len() != types.len() {
                    let (count, columns) = (formats.len(), types.len());
                    let message = format!(
                        "bind message has {count} result formats but query has {columns} columns"
                    );
                    return state.fail(Failure::new("08P01", message));
                }
                for (at, logical) in types.iter().enumerate() {
                    let format = match formats.len() {
                        0 => 0,
                        1 => formats[0],
                        _ => formats[at],
                    };
                    if format != 0 && format != 1 {
                        let message = format!("unsupported format code: {format}");
                        return state.fail(Failure::new("22023", message));
                    }
                    let ty = column_type(logical, origin(at));
                    columns.push((logical.clone(), ty.oid, format == 1));
                    typmods.push(ty.typmod);
                }
            }
        }
        // PostgreSQL sends the `RowDescription` before the statement runs, so the warnings of the
        // statement come after it.
        state.warnings();
        match RowEncoder::with_typmods(&columns, &typmods) {
            Ok(encoder) => {
                state.encoder = encoder;
                Ok(())
            }
            Err(error) => state.fail(type_failure(error)),
        }
    }

    fn rows(&mut self, chunk: Chunk) -> rudb::Result<()> {
        let mut state = self.0.borrow_mut();
        let state = &mut *state;
        let chunk = chunk.settled()?;
        let n = chunk.len();
        state.warnings();
        let zone = state.zone.get_or_insert_with(|| zone::of(&state.zone_name));
        let settings = output(&state.format, zone);
        if let Err(error) =
            state.encoder.encode(chunk.columns(), 0..n, &settings, state.out.bytes_mut())
        {
            return state.fail(type_failure(error));
        }
        state.rows += n as u64;
        if state.out.len() >= FLUSH_AT {
            state.flush()?;
        }
        Ok(())
    }
}

fn type_failure(error: TypeError) -> Failure {
    Failure::new(error.sqlstate.as_str(), error.message)
}
