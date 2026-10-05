//! One session, from the first byte of the client to the end of the connection.
//!
//! The thread reads with `poll(2)` on the socket and on its wake pipe. The startup reads packets
//! with [`Handshake`] until a `StartupMessage`, and the main loop reads messages with [`Session`],
//! which decides when `ReadyForQuery` goes out and which messages to drop. The output collects in
//! one buffer, which goes to the socket at `ReadyForQuery`, at `Flush`, at the end of the
//! connection, and when it is larger than [`FLUSH_AT`], as PostgreSQL does.
//!
//! This version has the simple and the extended query flows. The settings of the startup message
//! other than `application_name` and `client_encoding`, and authentication other than `trust`
//! come in the next steps of milestone PG1.

mod extended;

use std::io::{self, Read, Write};
use std::os::fd::AsRawFd;
use std::os::unix::net::UnixStream;
use std::sync::Arc;

use rudb::{Connection, ErrorCode, QueryResult, Transaction};
use rudb_pgtypes::{
    ByteaOutput, DateFormat, FixedZone, IntervalStyle, OutputSettings, RowEncoder, TypeInfo,
    pg_type,
};
use rudb_pgwire::{
    CommandTag, Field, Frontend, Handshake, Level, OutBuf, QUERY_CANCELED, Replication, Session,
    Step, TransactionStatus, split_startup,
};

use extended::Extended;

use crate::poll;
use crate::server::{Refusal, Shared, log};
use crate::stream::Stream;

/// The size of the output at which the session writes it to the socket before the end of the
/// result, the size of the send buffer of PostgreSQL.
const FLUSH_AT: usize = 64 << 10;

/// The size of each read from the socket.
const READ_SIZE: usize = 16 << 10;

/// The version that `server_version` gives. `libpq` and the drivers read the number before the
/// space, so they see version 19.0.
const SERVER_VERSION: &str = concat!("19.0 (rudb ", env!("CARGO_PKG_VERSION"), ")");

/// The bytes from the client that the session did not use yet.
#[derive(Default)]
struct Input {
    buf: Vec<u8>,
    head: usize,
}

impl Input {
    fn pending(&self) -> &[u8] {
        &self.buf[self.head..]
    }

    fn consume(&mut self, n: usize) {
        self.head += n;
        if self.head == self.buf.len() {
            self.buf.clear();
            self.head = 0;
        }
    }
}

/// What a wait for input found.
enum Filled {
    Data,
    Closed,
    Woken,
}

/// The socket, the wake pipe and the output of one session.
struct Wire {
    stream: Stream,
    wake: UnixStream,
    out: OutBuf,
}

impl Wire {
    /// Waits for bytes from the client or for a wake, and adds the bytes to `input`.
    fn fill(&mut self, input: &mut Input) -> io::Result<Filled> {
        if input.head > 0 && input.head * 2 >= input.buf.len() {
            input.buf.drain(..input.head);
            input.head = 0;
        }
        let [socket, wake] = poll::readable([self.stream.as_raw_fd(), self.wake.as_raw_fd()])?;
        if wake {
            let mut drop = [0u8; 64];
            let _ = self.wake.read(&mut drop)?;
            if !socket {
                return Ok(Filled::Woken);
            }
        }
        let len = input.buf.len();
        input.buf.resize(len + READ_SIZE, 0);
        let read = loop {
            match self.stream.read(&mut input.buf[len..]) {
                Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
                other => break other,
            }
        };
        input.buf.truncate(len + *read.as_ref().unwrap_or(&0));
        match read {
            Ok(0) => Ok(Filled::Closed),
            Ok(_) => Ok(Filled::Data),
            Err(e) if e.kind() == io::ErrorKind::ConnectionReset => Ok(Filled::Closed),
            Err(e) => Err(e),
        }
    }

    /// Writes the output to the socket.
    fn flush(&mut self) -> io::Result<()> {
        if !self.out.is_empty() {
            self.stream.write_all(self.out.as_bytes())?;
            self.out.consume(self.out.len());
        }
        Ok(())
    }

    /// Writes an error of the server with the severity `FATAL` and sends all the output.
    fn fatal(&mut self, (sqlstate, message): Refusal) -> io::Result<()> {
        self.out.error_response(&[
            (b'S', b"FATAL"),
            (b'V', b"FATAL"),
            (b'C', sqlstate.as_bytes()),
            (b'M', message.as_bytes()),
        ]);
        self.flush()
    }
}

/// Removes the session from the registry when the thread ends, also after a panic.
struct Registered<'a> {
    shared: &'a Shared,
    pid: i32,
}

impl Drop for Registered<'_> {
    fn drop(&mut self) {
        self.shared.unregister(self.pid);
    }
}

/// The values of a `StartupMessage` that the session keeps.
struct Start {
    protocol: u32,
    user: String,
    database: String,
    replication: Replication,
    application_name: String,
    client_encoding: Option<String>,
}

/// Runs one session to its end.
pub(crate) fn run(shared: &Arc<Shared>, stream: Stream) {
    let (wake, waker) = match UnixStream::pair() {
        Ok(pair) => pair,
        Err(e) => {
            log("LOG", &format!("could not create the wake pipe of a session: {e}"));
            return;
        }
    };
    let pid = shared.register(waker, stream.try_clone().ok());
    let _registered = Registered { shared, pid };
    let mut wire = Wire { stream, wake, out: OutBuf::new() };
    let mut input = Input::default();
    // An error of the socket ends the session. The client is gone or broke the connection, and
    // PostgreSQL logs nothing for most of these.
    if let Ok(Some(start)) = startup(shared, &mut wire, &mut input) {
        let _ = serve(shared, pid, start, &mut wire, &mut input);
    }
    let _ = wire.flush();
}

/// Reads packets until a `StartupMessage`. `None` means that the connection ends.
fn startup(shared: &Shared, wire: &mut Wire, input: &mut Input) -> io::Result<Option<Start>> {
    let mut handshake = Handshake::new(false);
    loop {
        let (step, used) = match split_startup(input.pending()) {
            Err(error) => {
                log("LOG", &error.message);
                return Ok(None);
            }
            Ok(None) => match wire.fill(input)? {
                Filled::Data => continue,
                Filled::Closed => return Ok(None),
                Filled::Woken if shared.stopping() => return Ok(None),
                Filled::Woken => continue,
            },
            Ok(Some((packet, used))) => (handshake.packet(packet, &mut wire.out), used),
        };
        let start = match step {
            Err(error) => {
                wire.out.protocol_error(&error, handshake.protocol());
                if error.level == Level::Log {
                    log("LOG", &error.message);
                }
                wire.flush()?;
                return Ok(None);
            }
            Ok(Step::Cancel(cancel)) => {
                shared.cancel(&cancel);
                return Ok(None);
            }
            Ok(Step::Answer { request, .. }) => {
                wire.flush()?;
                input.consume(used);
                if let Err(error) = request.check_buffered(input.pending().len()) {
                    wire.out.protocol_error(&error, handshake.protocol());
                    wire.flush()?;
                    return Ok(None);
                }
                continue;
            }
            Ok(Step::Start(request)) => {
                let text = |bytes: &[u8]| String::from_utf8_lossy(bytes).into_owned();
                let setting = |name: &[u8]| {
                    request.settings.iter().rev().find(|(n, _)| *n == name).map(|(_, v)| text(v))
                };
                Start {
                    protocol: request.protocol,
                    user: text(request.user),
                    database: text(request.database),
                    replication: request.replication,
                    application_name: setting(b"application_name").unwrap_or_default(),
                    client_encoding: setting(b"client_encoding"),
                }
            }
        };
        input.consume(used);
        return Ok(Some(start));
    }
}

/// The name of a client encoding that the session can serve, as PostgreSQL reports it.
fn client_encoding(name: &str) -> Option<&'static str> {
    let key: String =
        name.chars().filter(char::is_ascii_alphanumeric).map(|c| c.to_ascii_lowercase()).collect();
    match key.as_str() {
        "utf8" | "unicode" => Some("UTF8"),
        "sqlascii" => Some("SQL_ASCII"),
        _ => None,
    }
}

/// The checks after the startup message, the end of the startup, and the main loop.
fn serve(
    shared: &Shared,
    pid: i32,
    start: Start,
    wire: &mut Wire,
    input: &mut Input,
) -> io::Result<()> {
    if shared.stopping() {
        return wire.fatal(("57P03", "the database system is shutting down".to_owned()));
    }
    if start.replication != Replication::Off {
        return wire.fatal(("0A000", "replication connections are not supported".to_owned()));
    }
    let encoding = match &start.client_encoding {
        None => "UTF8",
        Some(name) => match client_encoding(name) {
            Some(encoding) => encoding,
            None => {
                return wire.fatal((
                    "22023",
                    format!("invalid value for parameter \"client_encoding\": \"{name}\""),
                ));
            }
        },
    };
    let key = match shared.admit(pid, start.protocol) {
        Ok(key) => key,
        Err(refusal) => return wire.fatal(refusal),
    };
    wire.out.authentication_ok();
    let database = match shared.database(&start.database) {
        Ok(database) => database,
        Err(refusal) => return wire.fatal(refusal),
    };
    let connection = database.connect();
    shared.attach(pid, connection.clone());
    // The order of the hash table of PostgreSQL 19, which sends the reported settings in the
    // order of its buckets.
    let reported: [(&str, &str); 15] = [
        ("IntervalStyle", "postgres"),
        ("search_path", "\"$user\", public"),
        ("is_superuser", "on"),
        ("standard_conforming_strings", "on"),
        ("session_authorization", &start.user),
        ("client_encoding", encoding),
        ("server_version", SERVER_VERSION),
        ("server_encoding", "UTF8"),
        ("in_hot_standby", "off"),
        ("integer_datetimes", "on"),
        ("TimeZone", "UTC"),
        ("application_name", &start.application_name),
        ("default_transaction_read_only", "off"),
        ("scram_iterations", "4096"),
        ("DateStyle", "ISO, MDY"),
    ];
    for (name, value) in reported {
        wire.out.parameter_status(name.as_bytes(), value.as_bytes());
    }
    key.write(&mut wire.out);
    let mut session = Session::new();
    session.set_utf8(encoding == "UTF8");
    let zone = FixedZone::utc();
    let settings = OutputSettings {
        date_format: DateFormat::ISO_MDY,
        interval_style: IntervalStyle::Postgres,
        extra_float_digits: 1,
        bytea_output: ByteaOutput::Hex,
        time_zone: &zone,
    };
    let mut runner =
        Runner { connection, settings, encoder: RowEncoder::default(), implicit: false };
    let mut extended = Extended::default();
    loop {
        if session.wants_ready() {
            let status = match runner.connection.transaction() {
                Transaction::Idle => TransactionStatus::Idle,
                Transaction::Open => TransactionStatus::Block,
                Transaction::Aborted => TransactionStatus::Failed,
            };
            session.ready_for_query(status, &mut wire.out);
            wire.flush()?;
        }
        let read = session.read(input.pending());
        let used = read.used;
        let failed = match read.message {
            None => {
                input.consume(used);
                match wire.fill(input)? {
                    Filled::Data => continue,
                    Filled::Closed => return Ok(()),
                    Filled::Woken if shared.stopping() => return terminated(wire),
                    Filled::Woken => continue,
                }
            }
            Some(Err(error)) => {
                wire.out.protocol_error(&error, start.protocol);
                if error.level != Level::Error {
                    if error.level == Level::Log {
                        log("LOG", &error.message);
                    }
                    return wire.flush();
                }
                true
            }
            Some(Ok(Frontend::Query(sql))) => {
                extended.simple_query();
                let failed = runner.query(sql, &mut wire.out, wire_flush(&mut wire.stream))?;
                extended.end_of_transaction(runner.connection.transaction());
                failed
            }
            Some(Ok(Frontend::Parse { name, sql, types })) => {
                let done = extended.parse(&runner, name, sql, types.iter(), &mut wire.out);
                failure(done, &mut wire.out, start.protocol)
            }
            Some(Ok(Frontend::Bind(bind))) => {
                let done = extended.bind(&runner, &bind, &mut wire.out);
                failure(done, &mut wire.out, start.protocol)
            }
            Some(Ok(Frontend::Describe { target, name })) => {
                let rest = &input.pending()[used..];
                let done = extended.describe(&mut runner, target, name, rest, &mut wire.out);
                failure(done, &mut wire.out, start.protocol)
            }
            Some(Ok(Frontend::Execute { portal, max_rows })) => {
                let rest = &input.pending()[used..];
                let mut flush = wire_flush(&mut wire.stream);
                let done = extended.execute(
                    &mut runner,
                    portal,
                    max_rows,
                    rest,
                    &mut wire.out,
                    &mut flush,
                )?;
                failure(done, &mut wire.out, start.protocol)
            }
            Some(Ok(Frontend::Close { target, name })) => {
                extended.close(target, name, &mut wire.out);
                false
            }
            Some(Ok(Frontend::Sync)) => {
                let ended = runner.end_implicit();
                extended.end_of_transaction(runner.connection.transaction());
                match ended {
                    Ok(()) => false,
                    Err(failure) => {
                        failure.write("", &mut wire.out);
                        true
                    }
                }
            }
            Some(Ok(Frontend::Flush)) => {
                wire.flush()?;
                false
            }
            Some(Ok(Frontend::Terminate)) => return Ok(()),
            Some(Ok(other)) => {
                let message = match other {
                    Frontend::FunctionCall(_) => "the function call protocol is not supported yet",
                    _ => "this message is not supported yet",
                };
                let error = rudb_pgwire::ProtocolError {
                    level: Level::Error,
                    sqlstate: "0A000",
                    message: message.to_owned(),
                    detail: None,
                    hint: None,
                };
                wire.out.protocol_error(&error, start.protocol);
                true
            }
        };
        input.consume(used);
        if failed {
            runner.abort_implicit();
            extended.end_of_transaction(runner.connection.transaction());
            if shared.stopping() {
                return terminated(wire);
            }
            if let Some(fatal) = session.recover() {
                wire.out.protocol_error(&fatal, start.protocol);
                return wire.flush();
            }
        }
        if wire.out.len() >= FLUSH_AT {
            wire.flush()?;
        }
    }
}

/// Writes the error of a message of the extended flow, and gives true when there was one.
fn failure(done: Result<(), extended::Problem>, out: &mut OutBuf, protocol: u32) -> bool {
    match done {
        Ok(()) => false,
        Err(problem) => {
            problem.write(out, protocol);
            true
        }
    }
}

/// The end of a session when the server stops.
fn terminated(wire: &mut Wire) -> io::Result<()> {
    wire.fatal(("57P01", "terminating connection due to administrator command".to_owned()))
}

/// A writer that sends the output to the socket in the middle of a result.
fn wire_flush(stream: &mut Stream) -> impl FnMut(&mut OutBuf) -> io::Result<()> + '_ {
    move |out: &mut OutBuf| {
        stream.write_all(out.as_bytes())?;
        out.consume(out.len());
        Ok(())
    }
}

/// What runs the statements of a session.
struct Runner<'a> {
    connection: Connection,
    settings: OutputSettings<'a>,
    encoder: RowEncoder,
    /// The server opened the transaction for a `Query` of more than one statement or for the
    /// extended flow, and it ends the transaction at the end of the `Query` or at `Sync`. This is
    /// the implicit transaction block of PostgreSQL.
    implicit: bool,
}

/// A statement of transaction control that the server runs itself, because PostgreSQL gives a
/// warning where the engine gives an error, and because the implicit transaction changes it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Control {
    Begin,
    Commit,
    Rollback,
}

impl Control {
    /// `BEGIN`, `START TRANSACTION`, `COMMIT`, `END`, `ROLLBACK` and `ABORT`. `COMMIT AND CHAIN`,
    /// `ROLLBACK TO` and the two-phase commands go to the engine.
    fn of(sql: &str) -> Option<Control> {
        let words = leading_words(sql);
        let word = |i: usize| words.get(i).map_or("", String::as_str);
        let plain = |i: usize| matches!(word(i), "" | "WORK" | "TRANSACTION");
        match word(0) {
            "BEGIN" => Some(Control::Begin),
            "START" if word(1) == "TRANSACTION" => Some(Control::Begin),
            "COMMIT" | "END" if plain(1) && word(2) != "AND" => Some(Control::Commit),
            "ROLLBACK" | "ABORT" if plain(1) && word(2) != "AND" => Some(Control::Rollback),
            _ => None,
        }
    }

    fn tag(self) -> CommandTag {
        match self {
            Control::Begin => CommandTag::Begin,
            Control::Commit => CommandTag::Commit,
            Control::Rollback => CommandTag::Rollback,
        }
    }
}

/// What a statement gave: the result of the engine, or the tag of a statement that the server
/// ran itself.
enum Outcome {
    Result(QueryResult),
    Done(CommandTag),
}

/// Writes a `NoticeResponse` with the severity `WARNING`.
fn warning(out: &mut OutBuf, sqlstate: &str, message: &str) {
    out.notice_response(&[
        (b'S', b"WARNING"),
        (b'V', b"WARNING"),
        (b'C', sqlstate.as_bytes()),
        (b'M', message.as_bytes()),
    ]);
}

/// The error for a statement in a failed transaction block.
fn aborted_failure() -> Failure {
    Failure {
        sqlstate: "25P02".to_owned(),
        message: "current transaction is aborted, commands ignored until end of transaction block"
            .to_owned(),
        fields: None,
        position: None,
    }
}

/// An error for an `ErrorResponse`, from the engine or from the encoder.
struct Failure {
    sqlstate: String,
    message: String,
    fields: Option<Box<rudb_common::Fields>>,
    /// The byte offset in the query string.
    position: Option<usize>,
}

impl Failure {
    fn engine(error: &rudb::Error, offset: usize) -> Failure {
        let message = if error.code() == ErrorCode::Interrupt && error.message() == "Interrupted!" {
            QUERY_CANCELED.1.to_owned()
        } else {
            error.message().to_owned()
        };
        Failure {
            sqlstate: error.reported_state().as_str().to_owned(),
            message,
            fields: error.fields().cloned().map(Box::new),
            position: error.span().map(|span| offset + span.start as usize),
        }
    }

    fn write(&self, sql: &str, out: &mut OutBuf) {
        let position = self.position.map(|at| {
            let mut at = at.min(sql.len());
            while !sql.is_char_boundary(at) {
                at -= 1;
            }
            (sql[..at].chars().count() + 1).to_string()
        });
        let mut fields: Vec<(u8, &[u8])> = vec![
            (b'S', b"ERROR"),
            (b'V', b"ERROR"),
            (b'C', self.sqlstate.as_bytes()),
            (b'M', self.message.as_bytes()),
        ];
        let f = self.fields.as_ref();
        let optional = [
            (b'D', f.and_then(|f| f.detail.as_ref())),
            (b'H', f.and_then(|f| f.hint.as_ref())),
            (b'P', position.as_ref()),
            (b'W', f.and_then(|f| f.context.as_ref())),
            (b's', f.and_then(|f| f.schema.as_ref())),
            (b't', f.and_then(|f| f.table.as_ref())),
            (b'c', f.and_then(|f| f.column.as_ref())),
            (b'd', f.and_then(|f| f.data_type.as_ref())),
            (b'n', f.and_then(|f| f.constraint.as_ref())),
            (b'R', f.and_then(|f| f.routine.as_ref())),
        ];
        fields.extend(optional.iter().filter_map(|(code, v)| v.map(|v| (*code, v.as_bytes()))));
        out.error_response(&fields);
    }
}

impl Runner<'_> {
    /// Opens the implicit transaction when no transaction is open.
    fn begin_implicit(&mut self) -> Result<(), Failure> {
        if self.connection.transaction() == Transaction::Idle {
            self.connection.execute("BEGIN").map_err(|e| Failure::engine(&e, 0))?;
            self.implicit = true;
        }
        Ok(())
    }

    /// Commits the implicit transaction, at the end of a `Query` and at `Sync`.
    fn end_implicit(&mut self) -> Result<(), Failure> {
        if std::mem::take(&mut self.implicit) && self.connection.transaction() != Transaction::Idle
        {
            self.connection.execute("COMMIT").map_err(|e| Failure::engine(&e, 0))?;
        }
        Ok(())
    }

    /// Rolls back the implicit transaction after an error. A transaction block that the client
    /// opened stays open, in the failed state.
    fn abort_implicit(&mut self) {
        if std::mem::take(&mut self.implicit) && self.connection.transaction() != Transaction::Idle
        {
            let _ = self.connection.execute("ROLLBACK");
        }
    }

    /// Runs one statement with the transaction rules of PostgreSQL. `run` runs it in the engine,
    /// and `offset` is the place of the statement in the query, for the position of an error.
    fn run(
        &mut self,
        control: Option<Control>,
        offset: usize,
        out: &mut OutBuf,
        run: impl FnOnce(&Connection) -> rudb::Result<QueryResult>,
    ) -> Result<Outcome, Failure> {
        let state = self.connection.transaction();
        let ends = matches!(control, Some(Control::Commit | Control::Rollback));
        if state == Transaction::Aborted && !ends {
            return Err(aborted_failure());
        }
        match (control, state) {
            (Some(Control::Begin), Transaction::Open) => {
                // A BEGIN in the implicit transaction makes it a transaction block.
                if !std::mem::take(&mut self.implicit) {
                    warning(out, "25001", "there is already a transaction in progress");
                }
                return Ok(Outcome::Done(CommandTag::Begin));
            }
            (Some(end @ (Control::Commit | Control::Rollback)), Transaction::Idle) => {
                warning(out, "25P01", "there is no transaction in progress");
                return Ok(Outcome::Done(end.tag()));
            }
            (Some(end @ (Control::Commit | Control::Rollback)), Transaction::Open)
                if self.implicit =>
            {
                self.implicit = false;
                let sql = if end == Control::Commit { "COMMIT" } else { "ROLLBACK" };
                self.connection.execute(sql).map_err(|e| Failure::engine(&e, 0))?;
                warning(out, "25P01", "there is no transaction in progress");
                return Ok(Outcome::Done(end.tag()));
            }
            _ => {}
        }
        run(&self.connection).map(Outcome::Result).map_err(|e| Failure::engine(&e, offset))
    }

    /// Runs a `Query` message: each statement in it, in order, until the first error. Gives true
    /// when there was an error.
    fn query(
        &mut self,
        sql: &[u8],
        out: &mut OutBuf,
        mut flush: impl FnMut(&mut OutBuf) -> io::Result<()>,
    ) -> io::Result<bool> {
        let Ok(sql) = std::str::from_utf8(sql) else {
            out.error_response(&[
                (b'S', b"ERROR"),
                (b'V', b"ERROR"),
                (b'C', b"22021"),
                (b'M', b"invalid byte sequence for encoding \"UTF8\""),
            ]);
            return Ok(true);
        };
        let statements = match rudb::statements(sql) {
            Ok(statements) => statements,
            Err(error) => {
                Failure::engine(&error, 0).write(sql, out);
                return Ok(true);
            }
        };
        if statements.is_empty() {
            out.empty_query_response();
            return Ok(false);
        }
        // A query of more than one statement runs in one transaction, as in PostgreSQL.
        let implicit = statements.len() > 1;
        for statement in statements {
            let before = self.connection.transaction();
            let control = Control::of(statement.sql());
            let started = if implicit { self.begin_implicit() } else { Ok(()) };
            let ran = started.and_then(|()| {
                self.run(control, statement.offset(), out, |c| c.execute(statement.sql()))
            });
            let result = match ran {
                Ok(Outcome::Result(result)) => result,
                Ok(Outcome::Done(tag)) => {
                    out.command_tag(tag, 0);
                    continue;
                }
                Err(failure) => {
                    failure.write(sql, out);
                    return Ok(true);
                }
            };
            let tag = command_tag(statement.sql(), &result, before);
            let rows = if let Some(changes) = result.changes() {
                changes as u64
            } else if result.width() > 0 || tag == CommandTag::Select {
                match self.rows(&result, out, &mut flush)? {
                    Ok(rows) => rows,
                    Err(failure) => {
                        failure.write(sql, out);
                        return Ok(true);
                    }
                }
            } else {
                0
            };
            out.command_tag(tag, rows);
        }
        if let Err(failure) = self.end_implicit() {
            failure.write(sql, out);
            return Ok(true);
        }
        Ok(false)
    }

    /// Writes the `RowDescription` and a `DataRow` for each row of a result, and gives the number
    /// of rows.
    fn rows(
        &mut self,
        result: &QueryResult,
        out: &mut OutBuf,
        flush: &mut impl FnMut(&mut OutBuf) -> io::Result<()>,
    ) -> io::Result<Result<u64, Failure>> {
        let types: Vec<_> = result.types().iter().map(pg_type).collect();
        let fields: Vec<Field<'_>> = result
            .names()
            .iter()
            .zip(&types)
            .map(|(name, ty)| Field {
                name: name.as_bytes(),
                table: 0,
                column: 0,
                type_oid: ty.oid,
                type_size: TypeInfo::get(ty.oid).map_or(-1, |info| info.len),
                type_modifier: ty.typmod,
                format: 0,
            })
            .collect();
        out.row_description(&fields);
        let columns: Vec<_> = result
            .types()
            .iter()
            .zip(&types)
            .map(|(logical, ty)| (logical.clone(), ty.oid, false))
            .collect();
        let type_failure = |error: rudb_pgtypes::TypeError| Failure {
            sqlstate: error.sqlstate.as_str().to_owned(),
            message: error.message,
            fields: None,
            position: None,
        };
        self.encoder = match RowEncoder::new(&columns) {
            Ok(encoder) => encoder,
            Err(error) => return Ok(Err(type_failure(error))),
        };
        let mut rows = 0u64;
        for chunk in result.chunks() {
            let chunk = match chunk.clone().settled() {
                Ok(chunk) => chunk,
                Err(error) => return Ok(Err(Failure::engine(&error, 0))),
            };
            let n = chunk.len();
            if let Err(error) =
                self.encoder.encode(chunk.columns(), 0..n, &self.settings, out.bytes_mut())
            {
                return Ok(Err(type_failure(error)));
            }
            rows += n as u64;
            if out.len() >= FLUSH_AT {
                flush(out)?;
            }
        }
        Ok(Ok(rows))
    }
}

/// The words at the start of a statement, in upper case, without the words that do not change
/// the command tag.
fn leading_words(sql: &str) -> Vec<String> {
    const FILLER: [&str; 7] =
        ["OR", "REPLACE", "TEMP", "TEMPORARY", "UNIQUE", "UNLOGGED", "RECURSIVE"];
    sql.split(|c: char| !c.is_ascii_alphabetic() && c != '_')
        .filter(|w| !w.is_empty())
        .take(6)
        .map(str::to_ascii_uppercase)
        .filter(|w| !FILLER.contains(&w.as_str()))
        .take(3)
        .collect()
}

/// The command tag of a statement that ran, from its first words as `CreateCommandTag` gives it
/// from the parse tree.
fn command_tag(sql: &str, result: &QueryResult, before: Transaction) -> CommandTag {
    let words = leading_words(sql);
    let first = words.first().map_or("", String::as_str);
    let starts_query = sql.trim_start().starts_with('(');
    match first {
        _ if starts_query => CommandTag::Select,
        "SELECT" | "VALUES" | "TABLE" | "FROM" => CommandTag::Select,
        "WITH" => match rudb::statement_kind(sql) {
            Some("InsertStatement") => CommandTag::Insert,
            Some("UpdateStatement") => CommandTag::Update,
            Some("DeleteStatement") => CommandTag::Delete,
            _ => CommandTag::Select,
        },
        "END" | "COMMIT" if before == Transaction::Aborted => CommandTag::Rollback,
        "END" => CommandTag::Commit,
        "ABORT" => CommandTag::Rollback,
        _ => (1..=words.len())
            .rev()
            .map(|n| CommandTag::from_name(&words[..n].join(" ")))
            .find(|tag| *tag != CommandTag::Unknown)
            .unwrap_or(if result.width() > 0 && result.changes().is_none() {
                CommandTag::Select
            } else {
                CommandTag::Unknown
            }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_leading_words_skip_the_filler() {
        assert_eq!(leading_words("create or replace temp view v as"), ["CREATE", "VIEW", "V"]);
        assert_eq!(leading_words("  drop table if exists t"), ["DROP", "TABLE", "IF"]);
        assert_eq!(leading_words("begin"), ["BEGIN"]);
    }

    #[test]
    fn the_transaction_control_that_the_server_runs() {
        assert_eq!(Control::of("begin"), Some(Control::Begin));
        assert_eq!(Control::of("START TRANSACTION READ ONLY"), Some(Control::Begin));
        assert_eq!(Control::of("commit work"), Some(Control::Commit));
        assert_eq!(Control::of("end"), Some(Control::Commit));
        assert_eq!(Control::of("abort transaction"), Some(Control::Rollback));
        assert_eq!(Control::of("commit and chain"), None);
        assert_eq!(Control::of("rollback work and no chain"), None);
        assert_eq!(Control::of("rollback to savepoint a"), None);
        assert_eq!(Control::of("commit prepared 'x'"), None);
        assert_eq!(Control::of("select 1"), None);
    }

    #[test]
    fn client_encodings_by_any_spelling() {
        assert_eq!(client_encoding("utf-8"), Some("UTF8"));
        assert_eq!(client_encoding("Unicode"), Some("UTF8"));
        assert_eq!(client_encoding("sql_ascii"), Some("SQL_ASCII"));
        assert_eq!(client_encoding("LATIN1"), None);
    }
}
