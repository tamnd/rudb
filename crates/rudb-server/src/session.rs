//! One session, from the first byte of the client to the end of the connection.
//!
//! The thread reads with `poll(2)` on the socket and on its wake pipe. The startup reads packets
//! with [`Handshake`] until a `StartupMessage`, and the main loop reads messages with [`Session`],
//! which decides when `ReadyForQuery` goes out and which messages to drop. The output collects in
//! one buffer, which goes to the socket at `ReadyForQuery`, at `Flush`, at the end of the
//! connection, and when it is larger than [`FLUSH_AT`], as PostgreSQL does.
//!
//! This version has the simple and the extended query flows, and the settings of PostgreSQL with
//! `SET`, `RESET`, `SHOW` and `ParameterStatus`, and the authentication methods of `pg_hba.conf`
//! in [`auth`].

mod auth;
mod database;
mod extended;
mod keywords;
mod role;
mod setting;
mod zone;

use std::io::{self, Read, Write};
use std::os::fd::AsRawFd;
use std::os::unix::net::UnixStream;
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use rudb::{Connection, ErrorCode, QueryResult, Transaction};
use rudb_common::guc::{self, Action, Origin, Settings};
use rudb_common::session::Postgres;
use rudb_pgtypes::{
    ByteaOutput, DateFormat, DateOrder, DateStyle, DateTimeInput, IntervalStyle, NoZones,
    OutputSettings, RowEncoder, TypeInfo, UNIX_TO_POSTGRES_USECS, ZoneAbbrevs, pg_type,
};
use rudb_pgwire::{
    CommandTag, Field, Frontend, Handshake, Level, OutBuf, QUERY_CANCELED, Replication, Session,
    Step, TransactionStatus, split_startup,
};

use extended::Extended;
use setting::Command;
use zone::Zone;

use crate::conf;
use crate::poll;
use crate::roles::{Catalog, Roles};
use crate::server::{Defaults, Refusal, Shared, log};
use crate::stream::Stream;
use crate::tls::{self, Tls};
use crate::x509;

/// The size of the output at which the session writes it to the socket before the end of the
/// result, the size of the send buffer of PostgreSQL.
const FLUSH_AT: usize = 64 << 10;

/// The size of each read from the socket.
const READ_SIZE: usize = 16 << 10;

/// The version that `server_version` gives. `libpq` and the drivers read the number before the
/// space, so they see version 19.0.
const SERVER_VERSION: &str = concat!("19.0 (rudb ", env!("CARGO_PKG_VERSION"), ")");

/// The text of `version()`, in the shape of `PG_VERSION_STR`. SQLAlchemy and DBeaver parse it.
const VERSION: &str = concat!(
    "PostgreSQL 19.0 (rudb ",
    env!("CARGO_PKG_VERSION"),
    ") on ",
    env!("RUDB_TARGET"),
    ", compiled by rustc ",
    env!("RUDB_RUSTC"),
    ", ",
    env!("RUDB_POINTER_WIDTH"),
    "-bit"
);

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
    /// The TLS configuration of the server when the connection came, when `ssl` was on. A
    /// reload does not change it for a connection that is open.
    tls: Option<Arc<Tls>>,
    /// The subject of the certificate of the client, when it sent one over TLS.
    peer: Option<x509::Subject>,
}

impl Wire {
    /// Waits for bytes from the client or for a wake, and adds the bytes to `input`.
    fn fill(&mut self, input: &mut Input) -> io::Result<Filled> {
        if input.head > 0 && input.head * 2 >= input.buf.len() {
            input.buf.drain(..input.head);
            input.head = 0;
        }
        let [socket, wake] = if self.stream.buffered() {
            [true, false]
        } else {
            poll::readable([self.stream.as_raw_fd(), self.wake.as_raw_fd()])?
        };
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
            // TLS gives `UnexpectedEof` when the client closes the socket without a
            // `close_notify`, which libpq does.
            Err(e)
                if matches!(
                    e.kind(),
                    io::ErrorKind::ConnectionReset | io::ErrorKind::UnexpectedEof
                ) =>
            {
                Ok(Filled::Closed)
            }
            Err(e) => Err(e),
        }
    }

    /// Starts TLS on the TCP socket. `early` holds the bytes of the handshake that the session
    /// already read, for direct TLS. False means that the handshake failed and the connection
    /// ends, as the log says.
    fn start_tls(&mut self, config: &Arc<rustls::ServerConfig>, early: &[u8]) -> io::Result<bool> {
        let Stream::Tcp(socket) = &self.stream else {
            return Ok(false);
        };
        let socket = socket.try_clone()?;
        match tls::accept(config, socket, early) {
            Ok(stream) => {
                // The TLS library checked the certificate against the root certificates.
                if let Some(certificate) = stream.conn.peer_certificates().and_then(<[_]>::first) {
                    match x509::subject(certificate) {
                        Ok(subject) => self.peer = Some(subject),
                        Err(message) => {
                            if let Some(message) = message {
                                log("LOG", message);
                            }
                            return Ok(false);
                        }
                    }
                }
                self.stream = Stream::Tls(Box::new(stream));
                Ok(true)
            }
            Err(message) => {
                log("LOG", &message);
                Ok(false)
            }
        }
    }

    /// Writes the output to the socket.
    fn flush(&mut self) -> io::Result<()> {
        if !self.out.is_empty() {
            self.stream.write_all(self.out.as_bytes())?;
            self.stream.flush()?;
            self.out.consume(self.out.len());
        }
        Ok(())
    }

    /// Writes an error of the server with the severity `FATAL` to the log and to the client, and
    /// sends all the output.
    fn fatal(&mut self, refusal: Refusal) -> io::Result<()> {
        self.fatal_with(refusal, None)
    }

    /// [`Wire::fatal`] with a `DETAIL` for the log only, as `errdetail_log` of PostgreSQL.
    fn fatal_with(&mut self, (sqlstate, message): Refusal, detail: Option<&str>) -> io::Result<()> {
        match detail {
            Some(detail) => log("FATAL", &format!("{message}\nDETAIL:  {detail}")),
            None => log("FATAL", &message),
        }
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
    /// The `options` parameter.
    options: Option<String>,
    /// The other parameters, which are settings, in the order of the packet.
    settings: Vec<(String, String)>,
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
    let mut wire = Wire { stream, wake, out: OutBuf::new(), tls: shared.tls(), peer: None };
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
    let tcp = matches!(wire.stream, Stream::Tcp(_));
    while input.pending().is_empty() {
        match wire.fill(input)? {
            Filled::Data => {}
            Filled::Closed => return Ok(None),
            Filled::Woken if shared.stopping() => return Ok(None),
            Filled::Woken => {}
        }
    }
    // Direct TLS, `ProcessSSLStartup` of PostgreSQL. Without TLS, and on a Unix socket, the
    // server closes the connection with no answer.
    let direct = input.pending()[0] == tls::HANDSHAKE_BYTE;
    if direct {
        let Some(tls) = wire.tls.clone().filter(|_| tcp) else {
            return Ok(None);
        };
        let early = input.pending().to_vec();
        input.consume(early.len());
        if !wire.start_tls(&tls.config, &early)? {
            return Ok(None);
        }
        let Stream::Tls(stream) = &wire.stream else {
            return Ok(None);
        };
        if stream.conn.alpn_protocol() != Some(tls::ALPN) {
            log(
                "LOG",
                "received direct SSL connection request without ALPN protocol negotiation \
                 extension",
            );
            return Ok(None);
        }
    }
    // After direct TLS an `SSLRequest` gets `N`, as in PostgreSQL.
    let mut handshake = Handshake::new(!direct && tcp && wire.tls.is_some());
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
            Ok(Step::Answer { request, tls }) => {
                wire.flush()?;
                input.consume(used);
                // The bytes that came before the handshake stay in `input`, and the check below
                // refuses them over TLS, as PostgreSQL does.
                if let Some(config) = wire.tls.clone().filter(|_| tls)
                    && !wire.start_tls(&config.config, &[])?
                {
                    return Ok(None);
                }
                if let Err(error) = request.check_buffered(input.pending().len()) {
                    wire.out.protocol_error(&error, handshake.protocol());
                    wire.flush()?;
                    return Ok(None);
                }
                continue;
            }
            Ok(Step::Start(request)) => {
                let text = |bytes: &[u8]| String::from_utf8_lossy(bytes).into_owned();
                Start {
                    protocol: request.protocol,
                    user: text(request.user),
                    database: text(request.database),
                    replication: request.replication,
                    options: request.options.map(text),
                    settings: request.settings.iter().map(|(n, v)| (text(n), text(v))).collect(),
                }
            }
        };
        input.consume(used);
        return Ok(Some(start));
    }
}

/// `pg_split_opts` and the switches of `process_postgres_switches` that set a parameter: the
/// names and values of `-c name=value` and `--name=value` in the `options` of the startup packet.
/// A backslash takes the next character as it is, also a space.
fn split_options(options: &str) -> Result<Vec<(String, String)>, String> {
    let mut words = Vec::new();
    let mut chars = options.chars().peekable();
    loop {
        while chars.peek().is_some_and(|c| c.is_ascii_whitespace()) {
            chars.next();
        }
        if chars.peek().is_none() {
            break;
        }
        let mut word = String::new();
        while let Some(c) = chars.next_if(|c| !c.is_ascii_whitespace()) {
            if c == '\\' {
                if let Some(next) = chars.next() {
                    word.push(next);
                }
            } else {
                word.push(c);
            }
        }
        words.push(word);
    }
    let mut settings = Vec::new();
    let mut words = words.into_iter();
    while let Some(word) = words.next() {
        let (switch, pair) = if word == "-c" {
            ("-c", words.next().ok_or_else(|| "option requires an argument -- 'c'".to_owned())?)
        } else if let Some(pair) = word.strip_prefix("--") {
            ("--", pair.to_owned())
        } else if let Some(pair) = word.strip_prefix("-c") {
            ("-c", pair.to_owned())
        } else {
            return Err(format!("invalid command-line argument for server process: {word}"));
        };
        let Some((name, value)) = pair.split_once('=') else {
            return Err(if switch == "--" {
                format!("--{pair} requires a value")
            } else {
                format!("-c {pair} requires a value")
            });
        };
        settings.push((name.replace('-', "_"), value.to_owned()));
    }
    Ok(settings)
}

/// The settings of a new session: the values that the server owns, the values of the
/// configuration files, the command line, then the `options` of the startup packet and its
/// other parameters, as `process_startup_options` applies them.
fn session_settings(
    start: &Start,
    defaults: &Defaults,
    ssl: bool,
    superuser: bool,
) -> Result<Settings, Refusal> {
    let mut settings = Settings::new(superuser);
    let internal = [
        ("server_version", SERVER_VERSION),
        ("server_encoding", "UTF8"),
        ("client_encoding", "UTF8"),
        ("is_superuser", if superuser { "on" } else { "off" }),
        ("session_authorization", &start.user),
        ("TimeZone", "UTC"),
        ("log_timezone", "UTC"),
        ("lc_messages", "C"),
        ("lc_monetary", "C"),
        ("lc_numeric", "C"),
        ("lc_time", "C"),
        // The values that PostgreSQL works out at startup or that initdb writes.
        ("max_stack_depth", "2MB"),
        ("timezone_abbreviations", "Default"),
        ("default_text_search_config", "pg_catalog.english"),
        ("huge_pages_status", "off"),
        ("io_max_concurrency", "64"),
        ("wal_buffers", "4MB"),
        ("commit_timestamp_buffers", "256kB"),
        ("subtransaction_buffers", "256kB"),
        ("transaction_buffers", "256kB"),
        ("ssl", if ssl { "on" } else { "off" }),
        ("ssl_library", "rustls"),
    ];
    for (name, value) in internal {
        settings.set_internal(name, value).map_err(|e| refusal(&e))?;
    }
    conf::start_session(&mut settings, &defaults.file);
    // The server checked the values of the command line when it started.
    for (name, value) in &defaults.args {
        let _ = settings.set_argument(name, value);
    }
    for (name, value) in &defaults.paths {
        let _ = settings.set_argument(name, value);
    }
    let options = match &start.options {
        Some(options) => split_options(options).map_err(|message| ("42601", message))?,
        None => Vec::new(),
    };
    for (name, value) in options.iter().chain(&start.settings) {
        settings.set(name, Some(value), Action::Set, Origin::Startup).map_err(|e| refusal(&e))?;
    }
    Ok(settings)
}

/// The `FATAL` error for an error of a setting at startup.
fn refusal(error: &rudb::Error) -> Refusal {
    let state = error.reported_state();
    let state = match state.as_str() {
        "0A000" => "0A000",
        "22023" => "22023",
        "42501" => "42501",
        "42602" => "42602",
        "42704" => "42704",
        "55P02" => "55P02",
        _ => "XX000",
    };
    (state, error.message().to_owned())
}

/// The checks after the startup message, the end of the startup, and the main loop.
fn serve(
    shared: &Arc<Shared>,
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
    let key = match shared.admit(pid, start.protocol) {
        Ok(key) => key,
        Err(refusal) => return wire.fatal(refusal),
    };
    let Some(notices) = auth::authenticate(shared, &start, wire, input)? else {
        return Ok(());
    };
    wire.out.authentication_ok();
    let role = match shared.login(pid, &start.user) {
        Ok(role) => role,
        Err(refusal) => return wire.fatal(refusal),
    };
    let (oid, database) = match shared.connect(pid, &start.database, &role) {
        Ok(found) => found,
        Err(refusal) => return wire.fatal(refusal),
    };
    let mut defaults = shared.defaults();
    let mut guc = match session_settings(&start, &defaults, wire.tls.is_some(), role.superuser) {
        Ok(guc) => guc,
        Err(refusal) => return wire.fatal(refusal),
    };
    let connection = database.connect();
    shared.attach(pid, connection.clone());
    // `EmitConnectionWarnings`, after the settings of the startup packet.
    let md5_warnings = guc.get("md5_password_warnings").is_none_or(|on| on == "on");
    for notice in notices.iter().filter(|notice| md5_warnings || !notice.md5) {
        wire.out.notice_response(&[
            (b'S', b"WARNING"),
            (b'V', b"WARNING"),
            (b'C', b"01000"),
            (b'M', notice.message.as_bytes()),
            (b'D', notice.detail.as_bytes()),
        ]);
    }
    for (name, value) in guc.startup_reports() {
        wire.out.parameter_status(name.as_bytes(), value.as_bytes());
    }
    key.write(&mut wire.out);
    let mut session = Session::new();
    let mut runner = Runner {
        connection,
        guc,
        roles: shared.roles.clone(),
        shared: shared.clone(),
        pid,
        database: oid,
        login: role.oid,
        format: Format::default(),
        zone: Zone::of("UTC"),
        zone_name: "UTC".to_owned(),
        utf8: true,
        seen: u64::MAX,
        encoder: RowEncoder::default(),
        implicit: false,
    };
    runner.refresh();
    session.set_utf8(runner.utf8);
    let mut extended = Extended::default();
    loop {
        if session.wants_ready() {
            let status = match runner.connection.transaction() {
                Transaction::Idle => TransactionStatus::Idle,
                Transaction::Open => TransactionStatus::Block,
                Transaction::Aborted => TransactionStatus::Failed,
            };
            runner.refresh();
            session.set_utf8(runner.utf8);
            let mut reports = runner.guc.reports();
            // PostgreSQL changes `is_superuser` inside the change of `session_authorization`, so
            // with the last change first it reports `session_authorization` first.
            let at = |name| reports.iter().position(|(held, _)| *held == name);
            if let (Some(user), Some(superuser)) = (at("session_authorization"), at("is_superuser"))
                && superuser < user
            {
                let report = reports.remove(user);
                reports.insert(superuser, report);
            }
            for (name, value) in reports {
                wire.out.parameter_status(name.as_bytes(), value.as_bytes());
            }
            session.ready_for_query(status, &mut wire.out);
            wire.flush()?;
        }
        let read = session.read(input.pending());
        let used = read.used;
        // A backend takes a reload after it reads a message and before it handles it.
        if read.message.is_some() {
            let now = shared.defaults();
            if !Arc::ptr_eq(&now, &defaults) {
                conf::reload_session(&mut runner.guc, &defaults.file, &now.file);
                defaults = now;
                runner.refresh();
                session.set_utf8(runner.utf8);
            }
        }
        let failed = match read.message {
            None => {
                input.consume(used);
                match wire.fill(input)? {
                    Filled::Data => continue,
                    Filled::Closed => return Ok(()),
                    Filled::Woken if shared.stopping() || shared.terminating(pid) => {
                        return terminated(wire);
                    }
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
            if shared.stopping() || shared.terminating(pid) {
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

/// The end of a session when the server stops, or when `DROP DATABASE ... WITH (FORCE)` ends
/// it.
fn terminated(wire: &mut Wire) -> io::Result<()> {
    wire.fatal(("57P01", "terminating connection due to administrator command".to_owned()))
}

/// A writer that sends the output to the socket in the middle of a result.
fn wire_flush(stream: &mut Stream) -> impl FnMut(&mut OutBuf) -> io::Result<()> + '_ {
    move |out: &mut OutBuf| {
        stream.write_all(out.as_bytes())?;
        stream.flush()?;
        out.consume(out.len());
        Ok(())
    }
}

/// The settings of the text output that come from the parameters of the session.
#[derive(Debug, Clone, Copy)]
struct Format {
    date_format: DateFormat,
    interval_style: IntervalStyle,
    extra_float_digits: i32,
    bytea_output: ByteaOutput,
}

impl Default for Format {
    fn default() -> Format {
        Format {
            date_format: DateFormat::ISO_MDY,
            interval_style: IntervalStyle::Postgres,
            extra_float_digits: 1,
            bytea_output: ByteaOutput::Hex,
        }
    }
}

/// The settings of the text output for the encoder.
fn output<'a>(format: &Format, zone: &'a Zone) -> OutputSettings<'a> {
    OutputSettings {
        date_format: format.date_format,
        interval_style: format.interval_style,
        extra_float_digits: format.extra_float_digits,
        bytea_output: format.bytea_output,
        time_zone: zone,
    }
}

/// What runs the statements of a session.
struct Runner {
    connection: Connection,
    /// The values of the parameters of PostgreSQL in the session.
    guc: Settings,
    /// The roles of the cluster.
    roles: Arc<Roles>,
    shared: Arc<Shared>,
    /// The process ID of the session.
    pid: i32,
    /// The OID of the database of the session.
    database: u32,
    /// The role that logged in, `GetAuthenticatedUserId`.
    login: u32,
    /// What the output takes from `guc`, made again when [`Settings::generation`] changes.
    format: Format,
    zone: Zone,
    zone_name: String,
    utf8: bool,
    /// The generation of `guc` that `format` and `zone` come from.
    seen: u64,
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
    fn new(sqlstate: &str, message: String) -> Failure {
        Failure { sqlstate: sqlstate.to_owned(), message, fields: None, position: None }
    }

    fn engine(error: &rudb::Error, offset: usize) -> Failure {
        let mut sqlstate = error.reported_state().as_str().to_owned();
        let message = if error.code() == ErrorCode::Interrupt && error.message() == "Interrupted!" {
            QUERY_CANCELED.1.to_owned()
        } else if let Some(name) = error
            .message()
            .strip_prefix("Setting with name \"")
            .and_then(|rest| rest.strip_suffix("\" does not exist"))
        {
            // The engine answers `SHOW` of an unknown name in the words of DuckDB.
            sqlstate = "42704".to_owned();
            format!("unrecognized configuration parameter \"{name}\"")
        } else {
            error.message().to_owned()
        };
        Failure {
            sqlstate,
            message,
            fields: error.fields().cloned().map(Box::new),
            position: error.span().map(|span| offset + span.start as usize),
        }
    }

    fn write(&self, sql: &str, out: &mut OutBuf) {
        let position = self.position.map(|at| position(sql, at));
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

/// The `P` field for the byte offset `at` in `sql`: the place of the character, from 1.
fn position(sql: &str, at: usize) -> String {
    let mut at = at.min(sql.len());
    while !sql.is_char_boundary(at) {
        at -= 1;
    }
    (sql[..at].chars().count() + 1).to_string()
}

impl Runner {
    /// The settings of the text output.
    fn output(&self) -> OutputSettings<'_> {
        output(&self.format, &self.zone)
    }

    /// Makes the output settings again from the parameters if one of them changed.
    fn refresh(&mut self) {
        if self.guc.generation() == self.seen {
            return;
        }
        self.sync_superuser();
        self.seen = self.guc.generation();
        let text = |name: &str| self.guc.get(name).unwrap_or_default();
        let datestyle = text("DateStyle");
        let (style, order) = datestyle.split_once(", ").unwrap_or(("ISO", "MDY"));
        self.format.date_format = DateFormat {
            style: match style {
                "SQL" => DateStyle::Sql,
                "Postgres" => DateStyle::Postgres,
                "German" => DateStyle::German,
                _ => DateStyle::Iso,
            },
            order: match order {
                "DMY" => DateOrder::Dmy,
                "YMD" => DateOrder::Ymd,
                _ => DateOrder::Mdy,
            },
        };
        self.format.interval_style = match text("IntervalStyle").as_str() {
            "postgres_verbose" => IntervalStyle::PostgresVerbose,
            "sql_standard" => IntervalStyle::SqlStandard,
            "iso_8601" => IntervalStyle::Iso8601,
            _ => IntervalStyle::Postgres,
        };
        self.format.extra_float_digits = text("extra_float_digits").parse().unwrap_or(1);
        self.format.bytea_output =
            if text("bytea_output") == "escape" { ByteaOutput::Escape } else { ByteaOutput::Hex };
        self.utf8 = text("client_encoding") != "SQL_ASCII";
        let zone_name = text("TimeZone");
        if zone_name != self.zone_name {
            self.zone = Zone::of(&zone_name);
            self.zone_name = zone_name;
        }
        let postgres = Postgres { settings: self.guc.clone(), version: VERSION.to_owned() };
        self.connection.set_postgres(Arc::new(postgres));
    }

    /// The session user, from `session_authorization`.
    fn session_oid(&self, catalog: &Catalog) -> u32 {
        let name = self.guc.get("session_authorization").unwrap_or_default();
        catalog.find(&name).map_or(self.login, |role| role.oid)
    }

    /// The current user: the role of `SET ROLE`, or the session user.
    fn current_oid(&self, catalog: &Catalog) -> u32 {
        let name = self.guc.get("role").unwrap_or_default();
        match catalog.find(&name) {
            Some(role) if name != "none" => role.oid,
            _ => self.session_oid(catalog),
        }
    }

    /// Makes `is_superuser` follow the current user.
    fn sync_superuser(&mut self) {
        let catalog = self.roles.snapshot();
        let superuser = catalog.superuser(self.current_oid(&catalog));
        let text = if superuser { "on" } else { "off" };
        if self.guc.get("is_superuser").as_deref() != Some(text) {
            let _ = self.guc.set_internal("is_superuser", text);
        }
        self.guc.set_superuser(superuser);
    }

    /// The end of a transaction for the settings, when no transaction is open after a statement.
    fn settle(&mut self, commit: bool) {
        if self.connection.transaction() == Transaction::Idle && !self.implicit {
            self.guc.end(commit);
            self.refresh();
        }
    }

    /// Runs a statement on the settings of the session.
    fn setting(
        &mut self,
        command: &Command,
        state: Transaction,
        sql: &str,
        offset: usize,
        out: &mut OutBuf,
    ) -> Result<Outcome, Failure> {
        let failure = |e: rudb::Error| Failure::engine(&e, 0);
        let tag = match command {
            Command::Set { name, value, local, reset } => {
                let text = match value {
                    Some(args) => Some(guc::flatten(name, args).map_err(failure)?),
                    None => None,
                };
                if *local && state == Transaction::Idle && !self.implicit {
                    warning(out, "25P01", "SET LOCAL can only be used in transaction blocks");
                }
                let action = if *local { Action::Local } else { Action::Set };
                self.guc.set(name, text.as_deref(), action, Origin::Statement).map_err(failure)?;
                if *reset { CommandTag::Reset } else { CommandTag::Set }
            }
            Command::ResetAll => {
                self.guc.reset_all();
                CommandTag::Reset
            }
            Command::Authorization { user, reset } => {
                let catalog = self.roles.snapshot();
                if let Some(user) = user {
                    let Some(role) = catalog.find(user) else {
                        return Err(Failure::new(
                            "22023",
                            format!("role \"{user}\" does not exist"),
                        ));
                    };
                    if role.oid != self.login && !catalog.superuser(self.login) {
                        return Err(Failure::new(
                            "42501",
                            format!("permission denied to set session authorization \"{user}\""),
                        ));
                    }
                }
                self.guc
                    .set("session_authorization", user.as_deref(), Action::Set, Origin::Statement)
                    .map_err(failure)?;
                if *reset { CommandTag::Reset } else { CommandTag::Set }
            }
            Command::Role(role) => {
                if role != "none" {
                    let catalog = self.roles.snapshot();
                    let Some(target) = catalog.find(role) else {
                        return Err(Failure::new(
                            "22023",
                            format!("role \"{role}\" does not exist"),
                        ));
                    };
                    if !catalog.can_set(self.session_oid(&catalog), target.oid) {
                        return Err(Failure::new(
                            "42501",
                            format!("permission denied to set role \"{role}\""),
                        ));
                    }
                }
                self.guc
                    .set("role", Some(role), Action::Set, Origin::Statement)
                    .map_err(failure)?;
                CommandTag::Set
            }
            Command::Roles(parsed) => {
                let catalog = self.roles.snapshot();
                let now = SystemTime::now()
                    .duration_since(UNIX_EPOCH)
                    .map_or(0, |since| i64::try_from(since.as_micros()).unwrap_or(0));
                let cx = role::Context {
                    roles: &self.roles,
                    current: self.current_oid(&catalog),
                    session: self.session_oid(&catalog),
                    guc: &self.guc,
                    databases: &self.shared.databases.snapshot(),
                    datetime: DateTimeInput {
                        order: self.format.date_format.order,
                        zone: &self.zone,
                        zones: &NoZones,
                        abbrevs: ZoneAbbrevs::postgres_default(),
                        now: now + UNIX_TO_POSTGRES_USECS,
                    },
                };
                let tag = role::execute(parsed, offset, &cx, out)?;
                self.sync_superuser();
                tag
            }
            Command::Databases(parsed) => {
                let catalog = self.roles.snapshot();
                let cx = database::Context {
                    shared: &self.shared,
                    pid: self.pid,
                    database: self.database,
                    current: self.current_oid(&catalog),
                    session: self.session_oid(&catalog),
                    block: state != Transaction::Idle,
                    sql,
                };
                database::execute(parsed, offset, &cx, out)?
            }
            Command::Show(name) => {
                let (column, value) = self.guc.show(name).map_err(failure)?;
                let result = QueryResult::text(vec![column], &[vec![value]]).map_err(failure)?;
                return Ok(Outcome::Result(result));
            }
            Command::ShowAll => {
                let rows: Vec<Vec<String>> = self
                    .guc
                    .show_all()
                    .map(|(name, value, description)| {
                        vec![name.to_owned(), value, description.to_owned()]
                    })
                    .collect();
                let names = ["name", "setting", "description"].map(str::to_owned).to_vec();
                let result = QueryResult::text(names, &rows).map_err(failure)?;
                return Ok(Outcome::Result(result));
            }
        };
        Ok(Outcome::Done(tag))
    }

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
            let committed = self.connection.execute("COMMIT");
            self.settle(committed.is_ok());
            committed.map_err(|e| Failure::engine(&e, 0))?;
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
        self.settle(false);
    }

    /// Runs one statement with the transaction rules of PostgreSQL. `run` runs it in the engine,
    /// and `offset` is the place of the statement in the query, for the position of an error.
    fn run(
        &mut self,
        control: Option<Control>,
        command: Option<&Command>,
        sql: &str,
        offset: usize,
        out: &mut OutBuf,
        run: impl FnOnce(&Connection) -> rudb::Result<QueryResult>,
    ) -> Result<Outcome, Failure> {
        let state = self.connection.transaction();
        let outcome = self.dispatch(control, command, sql, offset, out, run);
        // A statement that ends the transaction, or a statement outside of a transaction, ends
        // the transaction of the settings too.
        let commit =
            outcome.is_ok() && control != Some(Control::Rollback) && state != Transaction::Aborted;
        self.settle(commit);
        // PostgreSQL undoes the settings of a block when the block fails, not at its ROLLBACK.
        if outcome.is_err() && self.connection.transaction() == Transaction::Aborted {
            self.guc.end(false);
            self.refresh();
        }
        outcome
    }

    fn dispatch(
        &mut self,
        control: Option<Control>,
        command: Option<&Command>,
        sql: &str,
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
        if let Some(command) = command {
            let done = self.setting(command, state, sql, offset, out);
            if done.is_err() {
                self.connection.abort_transaction();
            }
            // The next statement can read the parameters, and it can be in the same message.
            self.refresh();
            return done;
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
            let command = setting::parse(statement.sql());
            let started = if implicit { self.begin_implicit() } else { Ok(()) };
            let ran = started.and_then(|()| {
                let offset = statement.offset();
                self.run(control, command.as_ref(), sql, offset, out, |c| {
                    c.execute(statement.sql())
                })
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
            .zip(result.types())
            .enumerate()
            .map(|(at, (name, ty))| field(name, ty, result.origin(at), 0))
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
            let settings = output(&self.format, &self.zone);
            if let Err(error) =
                self.encoder.encode(chunk.columns(), 0..n, &settings, out.bytes_mut())
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

/// The `RowDescription` field of a column of a name, a type, a source column and a format.
pub(super) fn field<'a>(
    name: &'a str,
    logical: &rudb_common::LogicalType,
    origin: Option<rudb_common::Origin>,
    format: i16,
) -> Field<'a> {
    let ty = pg_type(logical);
    // PostgreSQL numbers the columns of a table from one.
    let source = origin.and_then(|origin| {
        let column = origin.column.checked_add(1)?;
        Some((u32::try_from(origin.table).ok()?, i16::try_from(column).ok()?))
    });
    let (table, column) = source.unwrap_or((0, 0));
    Field {
        name: name.as_bytes(),
        table,
        column,
        type_oid: ty.oid,
        type_size: TypeInfo::get(ty.oid).map_or(-1, |info| info.len),
        type_modifier: ty.typmod,
        format,
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
    fn options_of_the_startup_packet() {
        let pair = |n: &str, v: &str| (n.to_owned(), v.to_owned());
        assert_eq!(
            split_options("-c work_mem=64MB --search-path=a\\ b -cjit=off").unwrap(),
            [pair("work_mem", "64MB"), pair("search_path", "a b"), pair("jit", "off")]
        );
        assert_eq!(split_options("-c work_mem").unwrap_err(), "-c work_mem requires a value");
        assert!(split_options("-x").is_err());
    }
}
