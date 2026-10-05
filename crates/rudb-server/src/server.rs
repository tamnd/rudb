//! The listeners, the acceptor thread, and the state that all sessions share.
//!
//! One thread accepts the connections of every listener and starts one thread for each session.
//! The sessions find each other through the registry, which a cancel request reads and which a
//! stop uses to wake every session. Each database is open once and stays open until the server
//! stops, so two sessions on one database share one `rudb::Database`.

use std::collections::{HashMap, HashSet};
use std::io::{self, Write};
use std::net::{SocketAddr, TcpListener, ToSocketAddrs};
use std::os::fd::{AsRawFd, RawFd};
use std::os::unix::fs::PermissionsExt;
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU8, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError, mpsc};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use rudb::{Connection, Database};
use rudb_pgwire::{
    CANCEL_KEY_LEN, Cancel, CancelKey, MOCK_NONCE_LEN, SCRAM_ITERATIONS, cancel_target,
};

use rudb_common::guc;

use crate::conf;
use crate::config::Config;
use crate::databases::{self, Catalog, Databases, Row};
use crate::hba::{self, Hba, Ident, ParseSettings};
use crate::poll;
use crate::roles::{self, Role, Roles};
use crate::session;
use crate::stream::Stream;
use crate::tls::{self, Tls};

/// The name of the lock file in the data directory, which holds the process ID of the server.
const PID_FILE: &str = "rudb-server.pid";

/// The file in the data directory with the mock nonce, in hexadecimal. PostgreSQL keeps the nonce
/// in `pg_control`.
const MOCK_NONCE_FILE: &str = "global/mock_auth_nonce";

/// The time that a stop gives the sessions to end by themselves before it closes their sockets.
const STOP_GRACE: Duration = Duration::from_secs(5);

/// The stack of a session thread, the same as the main thread of a process on Linux, because a
/// deep expression recurses in the parser and the binder.
const SESSION_STACK: usize = 8 << 20;

/// How many session threads can wait for a new connection after their session ends. A new
/// connection takes a waiting thread before the server starts a new one, because to start and
/// end a thread costs more than the rest of a short session.
const WAITING_THREADS: usize = 32;

/// How long a session thread waits for a new connection before it ends.
const THREAD_WAIT: Duration = Duration::from_secs(30);

/// Writes one message to the log, which is the standard error, in the format of PostgreSQL. The
/// message can hold `DETAIL`, `HINT` and `CONTEXT` lines, and each other line after the first
/// starts with a tab, as PostgreSQL writes a message of more than one line.
pub(crate) fn log(level: &str, message: &str) {
    let mut out = format!("{level}:  ");
    for (at, line) in message.split('\n').enumerate() {
        if at > 0 {
            out.push('\n');
            if !["DETAIL:  ", "HINT:  ", "CONTEXT:  "].iter().any(|label| line.starts_with(label)) {
                out.push('\t');
            }
        }
        out.push_str(line);
    }
    eprintln!("{out}");
}

/// A running server.
#[derive(Debug)]
pub struct Server {
    shared: Arc<Shared>,
    acceptor: Option<JoinHandle<()>>,
    /// The write end of the pipe that stops the acceptor.
    stop: UnixStream,
    addresses: Vec<SocketAddr>,
    sockets: Vec<PathBuf>,
    /// The files that the server removes when it stops: the sockets, their lock files and the
    /// lock file of the data directory.
    owned: Vec<PathBuf>,
}

/// One session in the registry.
#[derive(Debug)]
pub(crate) struct Entry {
    /// The key, from the end of the startup. A session that has no key yet does not count against
    /// `max_connections` and cannot be canceled.
    pub(crate) key: Option<CancelKey>,
    /// The connection to the database, which a cancel request interrupts.
    pub(crate) connection: Option<Connection>,
    /// The write end of the wake pipe of the session.
    wake: UnixStream,
    /// A second handle on the socket of the client, which a stop shuts down when the session does
    /// not end in time.
    stream: Option<Stream>,
    /// The role that logged in, for the connection limit of the role.
    role: Option<u32>,
    /// The OID of the database of the session, from the check of its connection limit.
    database: Option<u32>,
    /// True after `DROP DATABASE ... WITH (FORCE)` ended the session.
    terminate: bool,
}

/// The sessions by process ID.
#[derive(Debug)]
pub(crate) struct Sessions {
    next: i32,
    map: HashMap<i32, Entry>,
    /// The databases that a statement copies, renames or drops. A new session on one of them
    /// waits until the statement ends, as the lock on the database makes it wait in PostgreSQL.
    held: HashSet<u32>,
}

/// The values of the configuration files and of the command line that a session starts with.
#[derive(Debug)]
pub(crate) struct Defaults {
    /// The items of the files that the server took, in the order of the files.
    pub(crate) file: Vec<(String, String)>,
    /// The values that the server owns, the values of the files, the parameters of the command
    /// line, and `data_directory`, `config_file`, `hba_file` and `ident_file`, as one set of
    /// values. Each session starts from a copy of it, so a new session does not read the files
    /// and the parameters again.
    pub(crate) base: guc::Settings,
    /// True when the files or the command line give `ssl`. Then the session keeps that value and
    /// does not show whether its own connection uses TLS.
    pub(crate) ssl_given: bool,
}

/// The state that the acceptor and all sessions share.
#[derive(Debug)]
pub(crate) struct Shared {
    /// The configuration that the caller gave, under the values of the files.
    base: Config,
    /// The configuration with the values of the files, which a reload changes.
    config: Mutex<Arc<Config>>,
    /// The values of the command line and of the files, with the rules of the postmaster.
    state: Mutex<conf::State>,
    defaults: Mutex<Arc<Defaults>>,
    /// The TLS configuration, when `ssl` is on.
    tls: Mutex<Option<Arc<Tls>>>,
    /// The nonce of the cluster that makes the mock SCRAM secret of a role with no secret.
    pub(crate) mock_nonce: [u8; MOCK_NONCE_LEN],
    hba: Mutex<Arc<Hba>>,
    ident: Mutex<Arc<Ident>>,
    /// The roles of the cluster.
    pub(crate) roles: Arc<Roles>,
    /// The databases of the cluster.
    pub(crate) databases: Databases,
    /// The open databases by OID. The lock goes after the lock of the sessions and after the lock
    /// of the databases.
    open: Mutex<HashMap<u32, Arc<Database>>>,
    /// Lets one statement on the databases run at a time.
    ddl: Mutex<()>,
    sessions: Mutex<Sessions>,
    threads: Mutex<Vec<JoinHandle<()>>>,
    /// The session threads that wait for a new connection, each with a number and the sender of
    /// its connection. The newest one is last.
    waiting: Mutex<Waiting>,
    /// The shutdown in progress, as a [`Shutdown`] number, or 0 when the server runs.
    shutdown: AtomicU8,
}

/// The shutdown modes of PostgreSQL, in the order in which a later request takes over an earlier
/// one.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Shutdown {
    /// `SIGTERM`: no new sessions. The server stops when the last session ends.
    Smart = 1,
    /// `SIGINT`: the server ends each session with `57P01`, writes each database to its file
    /// and stops.
    Fast = 2,
    /// `SIGQUIT`: the server ends each session with a warning and stops without writing the
    /// databases to their files. The next start reads the journals.
    Immediate = 3,
}

/// A reason why a session cannot start, with the SQLSTATE and the text of PostgreSQL.
pub(crate) type Refusal = (&'static str, String);

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

impl Shared {
    /// The configuration of the server now.
    pub(crate) fn config(&self) -> Arc<Config> {
        lock(&self.config).clone()
    }

    /// The TLS configuration that new connections use.
    pub(crate) fn tls(&self) -> Option<Arc<Tls>> {
        lock(&self.tls).clone()
    }

    /// The values that new sessions start with, and that sessions take after a reload.
    pub(crate) fn defaults(&self) -> Arc<Defaults> {
        lock(&self.defaults).clone()
    }

    /// The reload after `SIGHUP`, `process_pm_reload_request`: the configuration files, then
    /// `pg_hba.conf` and `pg_ident.conf`, then TLS. A file that does not load leaves the old one
    /// in use.
    fn reload(&self) {
        log("LOG", "received SIGHUP, reloading configuration files");
        let mut lines = Vec::new();
        let config = {
            let mut state = lock(&self.state);
            state.reload(&mut lines);
            for line in lines.drain(..) {
                log("LOG", &line);
            }
            let config = match configure(&self.base, &state) {
                Ok(config) => Arc::new(config),
                Err(error) => {
                    log("LOG", &error);
                    self.config()
                }
            };
            *lock(&self.defaults) = Arc::new(defaults(&config, &state));
            config
        };
        *lock(&self.config) = config.clone();
        let (hba_path, ident_path) = auth_paths(&config);
        let hba = Hba::load(&hba_path, ParseSettings { ssl: config.ssl }, &mut lines);
        for line in lines.drain(..) {
            log("LOG", &line);
        }
        match hba {
            Some(hba) => *lock(&self.hba) = Arc::new(hba),
            None => log("LOG", &format!("{hba_path} was not reloaded")),
        }
        let ident = Ident::load(&ident_path, &mut lines);
        for line in lines.drain(..) {
            log("LOG", &line);
        }
        match ident {
            Some(ident) => *lock(&self.ident) = Arc::new(ident),
            None => log("LOG", &format!("{ident_path} was not reloaded")),
        }
        match tls::load(&config) {
            Ok(tls) => *lock(&self.tls) = tls,
            Err(error) => {
                log("LOG", &error);
                log("LOG", "SSL configuration was not reloaded");
            }
        }
    }

    /// The `pg_hba.conf` that new connections use.
    pub(crate) fn hba(&self) -> Arc<Hba> {
        lock(&self.hba).clone()
    }

    /// The `pg_ident.conf` that new connections use.
    pub(crate) fn ident(&self) -> Arc<Ident> {
        lock(&self.ident).clone()
    }

    /// True after a fast or an immediate shutdown started, which ends each session.
    pub(crate) fn stopping(&self) -> bool {
        self.shutdown.load(Ordering::Acquire) >= Shutdown::Fast as u8
    }

    /// True after a shutdown of any mode started, which refuses new sessions.
    pub(crate) fn refusing(&self) -> bool {
        self.shutdown.load(Ordering::Acquire) >= Shutdown::Smart as u8
    }

    /// True after an immediate shutdown started.
    pub(crate) fn immediate(&self) -> bool {
        self.shutdown.load(Ordering::Acquire) == Shutdown::Immediate as u8
    }

    /// Starts a shutdown, `pmdie` of PostgreSQL. A request for a mode that is not stronger than
    /// the mode in progress does nothing. Gives true when the mode changed.
    fn request(&self, mode: Shutdown) -> bool {
        if self.shutdown.fetch_max(mode as u8, Ordering::AcqRel) >= mode as u8 {
            return false;
        }
        match mode {
            Shutdown::Smart => log("LOG", "received smart shutdown request"),
            Shutdown::Fast => {
                log("LOG", "received fast shutdown request");
                log("LOG", "aborting any active transactions");
            }
            Shutdown::Immediate => log("LOG", "received immediate shutdown request"),
        }
        if mode >= Shutdown::Fast {
            let mut sessions = lock(&self.sessions);
            for entry in sessions.map.values_mut() {
                if let Some(connection) = &entry.connection {
                    connection.interrupt();
                }
                let _ = entry.wake.write_all(b"x");
            }
        }
        true
    }

    /// Adds a session that has just connected, and gives its process ID. `wake` is the write end
    /// of its wake pipe.
    pub(crate) fn register(&self, wake: UnixStream, stream: Option<Stream>) -> i32 {
        let mut sessions = lock(&self.sessions);
        let mut pid = sessions.next;
        while sessions.map.contains_key(&pid) {
            pid = if pid == i32::MAX { 1001 } else { pid + 1 };
        }
        sessions.next = if pid == i32::MAX { 1001 } else { pid + 1 };
        sessions.map.insert(
            pid,
            Entry {
                key: None,
                connection: None,
                wake,
                stream,
                role: None,
                database: None,
                terminate: false,
            },
        );
        pid
    }

    /// Gives the session its cancel key at the end of the startup, unless `max_connections`
    /// sessions already have one.
    pub(crate) fn admit(&self, pid: i32, protocol: u32) -> Result<CancelKey, Refusal> {
        let mut sessions = lock(&self.sessions);
        let started = sessions.map.values().filter(|entry| entry.key.is_some()).count();
        if started >= self.config().max_connections {
            return Err(("53300", "sorry, too many clients already".to_owned()));
        }
        let key = CancelKey::new(pid, protocol, poll::random::<CANCEL_KEY_LEN>());
        if let Some(entry) = sessions.map.get_mut(&pid) {
            entry.key = Some(key);
        }
        Ok(key)
    }

    /// The checks of `InitializeSessionUserId` on the role of a new session: the role exists, it
    /// can log in, and it has fewer sessions than its connection limit. A superuser has no limit.
    pub(crate) fn login(&self, pid: i32, user: &str) -> Result<Role, Refusal> {
        let catalog = self.roles.snapshot();
        let Some(role) = catalog.find(user) else {
            return Err(("28000", format!("role \"{user}\" does not exist")));
        };
        if !role.login {
            return Err(("28000", format!("role \"{user}\" is not permitted to log in")));
        }
        let mut sessions = lock(&self.sessions);
        if let Some(entry) = sessions.map.get_mut(&pid) {
            entry.role = Some(role.oid);
        }
        if !role.superuser && role.connlimit >= 0 {
            let count = sessions.map.values().filter(|entry| entry.role == Some(role.oid)).count();
            if count > usize::try_from(role.connlimit).unwrap_or(0) {
                return Err(("53300", format!("too many connections for role \"{user}\"")));
            }
        }
        Ok(role.clone())
    }

    /// Keeps a handle on the connection of a session, for cancel requests.
    pub(crate) fn attach(&self, pid: i32, connection: Connection) {
        if let Some(entry) = lock(&self.sessions).map.get_mut(&pid) {
            entry.connection = Some(connection);
        }
    }

    /// Removes a session that ended.
    pub(crate) fn unregister(&self, pid: i32) {
        lock(&self.sessions).map.remove(&pid);
    }

    /// Acts on a `CancelRequest`. The client gets no answer in any case, so a request that cancels
    /// nothing only writes a line to the log.
    pub(crate) fn cancel(&self, cancel: &Cancel<'_>) {
        let sessions = lock(&self.sessions);
        match cancel_target(cancel, |pid| sessions.map.get(&pid).and_then(|entry| entry.key)) {
            Ok(pid) => {
                if let Some(connection) = sessions.map.get(&pid).and_then(|e| e.connection.as_ref())
                {
                    connection.interrupt();
                }
            }
            Err(line) => log("LOG", &line),
        }
    }

    /// The checks of `InitializeSessionUserId` and `CheckMyDatabase` on the database of a new
    /// session: the database exists, it takes connections, and it has fewer sessions than its
    /// connection limit. A superuser has no limit. Gives the OID of the database and the database,
    /// which is open once for the whole server.
    pub(crate) fn connect(
        &self,
        pid: i32,
        name: &str,
        role: &Role,
    ) -> Result<(u32, Arc<Database>), Refusal> {
        let missing = || ("3D000", format!("database \"{name}\" does not exist"));
        let oid = loop {
            let Some(row) = self.databases.snapshot().find(name).cloned() else {
                if !self.config().auto_create_database {
                    return Err(missing());
                }
                self.create_empty(name, role.oid)?;
                continue;
            };
            let mut sessions = lock(&self.sessions);
            if sessions.held.contains(&row.oid) {
                drop(sessions);
                std::thread::sleep(Duration::from_millis(10));
                continue;
            }
            // The database can change before the lock of the sessions.
            let Some(row) =
                self.databases.snapshot().find(name).cloned().filter(|now| now.oid == row.oid)
            else {
                continue;
            };
            if !row.allow_connections {
                return Err((
                    "55000",
                    format!("database \"{name}\" is not currently accepting connections"),
                ));
            }
            if let Some(entry) = sessions.map.get_mut(&pid) {
                entry.database = Some(row.oid);
            }
            if !role.superuser && row.connlimit >= 0 {
                let count =
                    sessions.map.values().filter(|entry| entry.database == Some(row.oid)).count();
                if count > usize::try_from(row.connlimit).unwrap_or(0) {
                    return Err(("53300", format!("too many connections for database \"{name}\"")));
                }
            }
            break row.oid;
        };
        let database = self
            .handle(oid)
            .map_err(|error| ("XX000", format!("could not open database \"{name}\": {error}")))?;
        Ok((oid, database))
    }

    /// Makes a database for `auto_create_database`: a copy of `template1` with its settings, or an
    /// empty database when `template1` does not exist.
    fn create_empty(&self, name: &str, owner: u32) -> Result<(), Refusal> {
        let _ddl = self.ddl();
        let failed =
            |error: String| ("XX000", format!("could not create database \"{name}\": {error}"));
        let catalog = self.databases.snapshot();
        if catalog.find(name).is_some() {
            return Ok(());
        }
        let mut oid = catalog.next_oid();
        while self.file_conflict(oid) {
            oid += 1;
        }
        let row = match catalog.find("template1") {
            Some(source) => {
                self.copy_database(source.oid, oid).map_err(failed)?;
                Row {
                    oid,
                    name: name.to_owned(),
                    owner,
                    template: false,
                    allow_connections: true,
                    connlimit: -1,
                    ..source.clone()
                }
            }
            None => {
                self.handle(oid).map_err(failed)?;
                Row::new(oid, name, owner)
            }
        };
        self.databases.change(
            |catalog| {
                catalog.rows.push(row);
                Ok(())
            },
            failed,
        )
    }

    /// The open database with this OID. The first call opens it, and makes an empty database when
    /// the file does not exist.
    fn handle(&self, oid: u32) -> Result<Arc<Database>, String> {
        let mut open = lock(&self.open);
        if let Some(database) = open.get(&oid) {
            return Ok(database.clone());
        }
        let path = databases::path(&self.config().data, oid);
        let text = path.to_str().ok_or("the path is not valid UTF-8")?;
        let database = Arc::new(Database::open(text).map_err(|error| error.message().to_owned())?);
        open.insert(oid, database.clone());
        Ok(database)
    }

    /// Lets one statement on the databases run at a time.
    pub(crate) fn ddl(&self) -> MutexGuard<'_, ()> {
        lock(&self.ddl)
    }

    /// Stops new sessions on the database until the guard goes.
    pub(crate) fn hold(&self, oid: u32) -> Hold<'_> {
        lock(&self.sessions).held.insert(oid);
        Hold { shared: self, oid }
    }

    /// The roles of the other sessions on the database.
    pub(crate) fn others(&self, oid: u32, me: i32) -> Vec<u32> {
        let sessions = lock(&self.sessions);
        sessions
            .map
            .iter()
            .filter(|(pid, entry)| **pid != me && entry.database == Some(oid))
            .filter_map(|(_, entry)| entry.role)
            .collect()
    }

    /// The number of other sessions on the database. When there are some, it waits up to five
    /// seconds for them to end, as `CountOtherDBBackends` does.
    pub(crate) fn wait_others(&self, oid: u32, me: i32) -> usize {
        let count = || {
            let sessions = lock(&self.sessions);
            sessions
                .map
                .iter()
                .filter(|(pid, entry)| **pid != me && entry.database == Some(oid))
                .count()
        };
        for _ in 0..50 {
            let others = count();
            if others == 0 {
                return 0;
            }
            std::thread::sleep(Duration::from_millis(100));
        }
        count()
    }

    /// Ends the other sessions on the database, as `TerminateOtherDBBackends` does.
    pub(crate) fn terminate(&self, oid: u32, me: i32) {
        let mut sessions = lock(&self.sessions);
        for (_, entry) in sessions
            .map
            .iter_mut()
            .filter(|(pid, entry)| **pid != me && entry.database == Some(oid))
        {
            entry.terminate = true;
            if let Some(connection) = &entry.connection {
                connection.interrupt();
            }
            let _ = entry.wake.write_all(b"x");
        }
    }

    /// True after another session ended this session.
    pub(crate) fn terminating(&self, pid: i32) -> bool {
        lock(&self.sessions).map.get(&pid).is_some_and(|entry| entry.terminate)
    }

    /// True when a file of a database with this OID exists, which a new database cannot take.
    pub(crate) fn file_conflict(&self, oid: u32) -> bool {
        let path = databases::path(&self.config().data, oid);
        path.exists() || databases::journal(&path).exists()
    }

    /// Copies the database `from` to a new file for the database `to`. The copy has all the
    /// changes up to now, because the copy writes the open database to its file first.
    pub(crate) fn copy_database(&self, from: u32, to: u32) -> Result<(), String> {
        let data = self.config().data.clone();
        let source = databases::path(&data, from);
        let target = databases::path(&data, to);
        let database = self.handle(from)?;
        Database::clone(&database).close().map_err(|error| {
            format!("could not write database file \"{}\": {}", source.display(), error.message())
        })?;
        let temp = target.with_extension("rudb.tmp");
        let copied = std::fs::copy(&source, &temp)
            .and_then(|_| std::fs::File::open(&temp)?.sync_all())
            .and_then(|()| std::fs::rename(&temp, &target));
        copied.map_err(|error| {
            let _ = std::fs::remove_file(&temp);
            format!(
                "could not copy file \"{}\" to \"{}\": {error}",
                source.display(),
                target.display()
            )
        })
    }

    /// Removes the files of a database that is no more in the catalog.
    pub(crate) fn remove_database(&self, oid: u32) -> Result<(), String> {
        let database = lock(&self.open).remove(&oid);
        drop(database);
        let file = databases::path(&self.config().data, oid);
        let journal = databases::journal(&file);
        let ignore = |result: io::Result<()>| match result {
            Err(error) if error.kind() != io::ErrorKind::NotFound => Err(error),
            _ => Ok(()),
        };
        ignore(std::fs::remove_file(&file))
            .and_then(|()| ignore(std::fs::remove_dir_all(&journal)))
            .map_err(|error| {
                format!("could not remove database file \"{}\": {error}", file.display())
            })
    }
}

/// A database that new sessions wait for, from [`Shared::hold`].
pub(crate) struct Hold<'a> {
    shared: &'a Shared,
    oid: u32,
}

impl Drop for Hold<'_> {
    fn drop(&mut self) {
        lock(&self.shared.sessions).held.remove(&self.oid);
    }
}

/// The settings of the server that the files can set.
const SERVER_PARAMETERS: [&str; 13] = [
    "listen_addresses",
    "port",
    "unix_socket_directories",
    "unix_socket_permissions",
    "max_connections",
    "ssl",
    "ssl_cert_file",
    "ssl_key_file",
    "ssl_ca_file",
    "ssl_min_protocol_version",
    "ssl_max_protocol_version",
    "hba_file",
    "ident_file",
];

/// The configuration of the server: the configuration that the caller gave, with the values of
/// the files for the settings that the command line did not set.
fn configure(base: &Config, state: &conf::State) -> Result<Config, String> {
    let mut config = base.clone();
    for name in SERVER_PARAMETERS {
        if !base.in_args(name)
            && let Some(value) = state.file_value(name)
        {
            config.assign(name, &value)?;
        }
    }
    Ok(config)
}

/// The values that a session starts with.
fn defaults(config: &Config, state: &conf::State) -> Defaults {
    let (hba, ident) = auth_paths(config);
    let args: Vec<(String, String)> = config
        .args
        .iter()
        .filter(|(name, _)| guc::find(name).is_some() || name.contains('.'))
        .cloned()
        .collect();
    let file = state.applied().to_vec();
    let paths = vec![
        ("data_directory", state.data().to_owned()),
        ("config_file", state.config_file().to_owned()),
        ("hba_file", hba),
        ("ident_file", ident),
    ];
    let base = session::base_settings(&file, &args, &paths);
    let ssl_given = file.iter().chain(&args).any(|(name, _)| name.eq_ignore_ascii_case("ssl"));
    Defaults { file, base, ssl_given }
}

/// The paths of `pg_hba.conf` and `pg_ident.conf`, absolute, as PostgreSQL logs them.
fn auth_paths(config: &Config) -> (String, String) {
    let path = |file: &Path, default: &str| {
        let file = if file.as_os_str().is_empty() { Path::new(default) } else { file };
        hba::config_path(&config.data, file).to_string_lossy().into_owned()
    };
    (path(&config.hba_file, "pg_hba.conf"), path(&config.ident_file, "pg_ident.conf"))
}

/// Loads `pg_hba.conf` and `pg_ident.conf` at start, as `PostmasterMain` does. The details of
/// each error go to the log. A `pg_hba.conf` that does not load stops the start, and a
/// `pg_ident.conf` that does not load leaves no maps.
fn load_auth_files(config: &Config) -> Result<(Hba, Ident), String> {
    let (hba_path, ident_path) = auth_paths(config);
    let mut lines = Vec::new();
    let hba = Hba::load(&hba_path, ParseSettings { ssl: config.ssl }, &mut lines);
    for line in lines.drain(..) {
        log("LOG", &line);
    }
    let hba = hba.ok_or_else(|| format!("could not load {hba_path}"))?;
    let ident = Ident::load(&ident_path, &mut lines);
    for line in lines {
        log("LOG", &line);
    }
    Ok((hba, ident.unwrap_or_default()))
}

/// The mock nonce of the cluster, from its file, or a new one in the file of a data directory
/// from an older version.
fn mock_nonce(data: &Path) -> Result<[u8; MOCK_NONCE_LEN], String> {
    let path = data.join(MOCK_NONCE_FILE);
    let parse = |text: &str| -> Option<[u8; MOCK_NONCE_LEN]> {
        let text = text.trim();
        let mut nonce = [0; MOCK_NONCE_LEN];
        if text.len() != 2 * MOCK_NONCE_LEN {
            return None;
        }
        for (at, byte) in nonce.iter_mut().enumerate() {
            *byte = u8::from_str_radix(text.get(2 * at..2 * at + 2)?, 16).ok()?;
        }
        Some(nonce)
    };
    match std::fs::read_to_string(&path) {
        Ok(text) => parse(&text)
            .ok_or_else(|| format!("invalid mock authentication nonce in \"{}\"", path.display())),
        Err(e) if e.kind() == io::ErrorKind::NotFound => write_mock_nonce(data),
        Err(e) => Err(format!("could not open file \"{}\": {e}", path.display())),
    }
}

/// Makes the mock nonce of a cluster and writes its file.
fn write_mock_nonce(data: &Path) -> Result<[u8; MOCK_NONCE_LEN], String> {
    let nonce = poll::random::<MOCK_NONCE_LEN>();
    let path = data.join(MOCK_NONCE_FILE);
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)
            .map_err(|e| format!("could not create directory \"{}\": {e}", dir.display()))?;
    }
    let text: String = nonce.iter().map(|b| format!("{b:02x}")).collect();
    std::fs::write(&path, text + "\n")
        .map_err(|e| format!("could not write file \"{}\": {e}", path.display()))?;
    Ok(nonce)
}

/// The sample files that `init` writes into the data directory.
const HBA_SAMPLE: &str = include_str!("../vendor/pg_hba.conf.sample");
const IDENT_SAMPLE: &str = include_str!("../vendor/pg_ident.conf.sample");

/// The comment that `initdb` puts at the top of the entries when a method is `trust`.
const TRUST_COMMENT: &str = "# CAUTION: Configuring the system for local \"trust\" authentication\n\
                             # allows any local user to connect as any PostgreSQL user, including\n\
                             # the database superuser.  If you do not trust all your local users,\n\
                             # use another authentication method.\n";

/// The methods that `initdb` accepts for `local` lines and for `host` lines.
const LOCAL_METHODS: [&str; 6] = ["trust", "reject", "scram-sha-256", "md5", "password", "peer"];
const HOST_METHODS: [&str; 6] = ["trust", "reject", "scram-sha-256", "md5", "password", "ident"];

/// The options of `init`, the options of `initdb` that it knows.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Init {
    /// The name of the bootstrap superuser.
    pub superuser: String,
    /// The password of the superuser in clear text, which the role file keeps as a SCRAM secret.
    pub password: Option<String>,
    /// The method for `local` lines, `trust` when it is `None`.
    pub auth_local: Option<String>,
    /// The method for `host` lines, `trust` when it is `None`.
    pub auth_host: Option<String>,
}

impl Init {
    /// The options for the superuser `superuser` with the defaults of `initdb`.
    pub fn new(superuser: &str) -> Init {
        Init { superuser: superuser.to_owned(), ..Init::default() }
    }

    /// `-A`: sets the method of both kinds of lines. `ident` for `host` lines is `peer` for
    /// `local` lines, and the reverse, as in `initdb`.
    pub fn auth(&mut self, method: &str) {
        self.auth_local = Some(method.to_owned());
        self.auth_host = Some(method.to_owned());
        if method == "ident" {
            self.auth_local = Some("peer".to_owned());
        } else if method == "peer" {
            self.auth_host = Some("ident".to_owned());
        }
    }

    /// True when a method was not given, so `initdb` uses `trust` and warns.
    pub fn trust_warning(&self) -> bool {
        self.auth_local.is_none() || self.auth_host.is_none()
    }

    /// The methods for `local` and `host` lines, after the checks of `initdb`.
    fn methods(&self) -> Result<(&str, &str), String> {
        let local = self.auth_local.as_deref().unwrap_or("trust");
        let host = self.auth_host.as_deref().unwrap_or("trust");
        for (method, valid, kind) in
            [(local, &LOCAL_METHODS, "local"), (host, &HOST_METHODS, "host")]
        {
            if !valid.contains(&method) {
                return Err(format!(
                    "invalid authentication method \"{method}\" for \"{kind}\" connections"
                ));
            }
        }
        let password = |method| matches!(method, "md5" | "password" | "scram-sha-256");
        if password(local) && password(host) && self.password.is_none() {
            return Err(
                "must specify a password for the superuser to enable password authentication"
                    .to_owned(),
            );
        }
        Ok((local, host))
    }
}

/// Makes a data directory with the databases `postgres`, `template1` and `template0`, the role
/// file, `postgresql.conf`, `postgresql.auto.conf`, `pg_hba.conf` and `pg_ident.conf`, as
/// `initdb` does.
///
/// # Errors
///
/// A superuser name that starts with `pg_`, a method that `initdb` does not accept, a password
/// method without a password, a data directory that exists and is not empty, or a file that the
/// server cannot write.
pub fn init(data: &Path, options: &Init) -> Result<(), String> {
    let superuser = options.superuser.as_str();
    if superuser.starts_with("pg_") {
        return Err(format!(
            "superuser name \"{superuser}\" is disallowed; role names cannot begin with \"pg_\""
        ));
    }
    let (local, host) = options.methods()?;
    if data.exists() && data.read_dir().map_err(|e| e.to_string())?.next().is_some() {
        return Err(format!("directory \"{}\" exists but is not empty", data.display()));
    }
    let base = data.join("base");
    std::fs::create_dir_all(&base)
        .map_err(|e| format!("could not create directory \"{}\": {e}", base.display()))?;
    std::fs::set_permissions(data, std::fs::Permissions::from_mode(0o700))
        .map_err(|e| format!("could not change permissions of \"{}\": {e}", data.display()))?;
    let catalog = Catalog::bootstrap();
    for row in &catalog.rows {
        let path = databases::path(data, row.oid);
        let text = path.to_str().ok_or_else(|| format!("bad path \"{}\"", path.display()))?;
        Database::open(text)
            .and_then(Database::close)
            .map_err(|e| format!("could not create database \"{}\": {}", row.name, e.message()))?;
    }
    write_mock_nonce(data)?;
    let postgresql = conf::sample(local, host);
    let comment = if local == "trust" || host == "trust" { TRUST_COMMENT } else { "" };
    let hba = hba::fill_sample(
        HBA_SAMPLE,
        &[("@authmethodhost@", host), ("@authmethodlocal@", local), ("@authcomment@", comment)],
    );
    for (name, text) in [
        ("postgresql.conf", postgresql.as_str()),
        (conf::AUTO_FILE, conf::AUTO_TEXT),
        ("pg_hba.conf", hba.as_str()),
        ("pg_ident.conf", IDENT_SAMPLE),
    ] {
        let path = data.join(name);
        std::fs::write(&path, text)
            .and_then(|()| std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)))
            .map_err(|e| format!("could not write file \"{}\": {e}", path.display()))?;
    }
    let password =
        options.password.as_deref().map(|password| roles::scram(password, SCRAM_ITERATIONS));
    roles::write(data, &roles::Catalog::bootstrap(superuser, password))?;
    databases::write(data, &catalog)
}

/// A listening socket.
enum Listener {
    Tcp(TcpListener),
    Unix(UnixListener),
}

impl Listener {
    fn fd(&self) -> RawFd {
        match self {
            Listener::Tcp(l) => l.as_raw_fd(),
            Listener::Unix(l) => l.as_raw_fd(),
        }
    }

    /// The next connection, or `None` when there is none now.
    fn accept(&self) -> io::Result<Option<Stream>> {
        let accepted = match self {
            Listener::Tcp(l) => l.accept().map(|(s, _)| {
                // A failure here leaves a slower connection that still works.
                let _ = s.set_nodelay(true);
                let _ = set_keepalive(s.as_raw_fd());
                Stream::Tcp(s)
            }),
            Listener::Unix(l) => l.accept().map(|(s, _)| Stream::Unix(s)),
        };
        match accepted {
            Ok(stream) => {
                // On macOS a socket keeps the non-blocking flag of its listener.
                match &stream {
                    Stream::Tcp(s) => s.set_nonblocking(false)?,
                    Stream::Unix(s) => s.set_nonblocking(false)?,
                    Stream::Tls(s) => s.sock.set_nonblocking(false)?,
                }
                Ok(Some(stream))
            }
            Err(e) if e.kind() == io::ErrorKind::WouldBlock => Ok(None),
            Err(e) => Err(e),
        }
    }
}

/// `SO_KEEPALIVE`, which PostgreSQL sets on each TCP connection.
fn set_keepalive(fd: RawFd) -> io::Result<()> {
    let on: libc::c_int = 1;
    // SAFETY: `on` is a valid `c_int` for the whole call, and its size is the length given.
    let done = unsafe {
        libc::setsockopt(
            fd,
            libc::SOL_SOCKET,
            libc::SO_KEEPALIVE,
            (&raw const on).cast(),
            size_of::<libc::c_int>() as libc::socklen_t,
        )
    };
    if done == 0 { Ok(()) } else { Err(io::Error::last_os_error()) }
}

/// True when a process with this ID runs.
fn alive(pid: i32) -> bool {
    // SAFETY: signal 0 only checks that the process exists and sends nothing.
    pid > 0
        && (unsafe { libc::kill(pid, 0) } == 0
            || io::Error::last_os_error().raw_os_error() == Some(libc::EPERM))
}

/// Makes a lock file that holds `lines`, after it checks that no live process owns the file.
fn lock_file(path: &Path, lines: &str, what: &str) -> Result<(), String> {
    if let Ok(old) = std::fs::read_to_string(path) {
        let pid = old.lines().next().and_then(|line| line.trim().parse::<i32>().ok());
        if let Some(pid) = pid.filter(|&pid| alive(pid)) {
            return Err(format!(
                "lock file \"{}\" already exists\nHINT:  Is another rudb-server (PID {pid}) \
                 using {what}?",
                path.display()
            ));
        }
    }
    std::fs::write(path, lines)
        .map_err(|e| format!("could not create lock file \"{}\": {e}", path.display()))
}

impl Server {
    /// Reads the configuration files, opens the listeners and starts to accept connections.
    /// The values of `postgresql.conf` and `postgresql.auto.conf` change the settings of
    /// `config` that its command line, [`Config::set`], did not set.
    ///
    /// # Errors
    ///
    /// A configuration file that is missing or has errors, a data directory that is not there or
    /// that another server uses, an address or a socket that the server cannot bind, or no
    /// listener at all.
    pub fn start(base: Config) -> Result<Server, String> {
        let mut state = conf::State::new(&base.data, &base.args);
        let mut lines = Vec::new();
        let started = state.start(&mut lines);
        for line in lines {
            log("LOG", &line);
        }
        started?;
        let config = configure(&base, &state)?;
        if !config.data.join("base").is_dir() {
            return Err(format!(
                "\"{}\" is not a valid data directory\nDETAIL:  The directory \"base\" is \
                 missing. Run rudb-server init first.",
                config.data.display()
            ));
        }
        let tls = tls::load(&config)?;
        let mock_nonce = mock_nonce(&config.data)?;
        let roles = Arc::new(Roles::open(&config.data)?);
        let databases = Databases::open(&config.data)?;
        let mut owned = Vec::new();
        let pid_file = config.data.join(PID_FILE);
        let me = std::process::id();
        lock_file(
            &pid_file,
            &format!("{me}\n{}\n", config.data.display()),
            &format!("data directory \"{}\"", config.data.display()),
        )?;
        owned.push(pid_file);
        let opened = Server::listen(&config, &mut owned);
        let opened = opened.and_then(|opened| Ok((opened, load_auth_files(&config)?)));
        let ((listeners, addresses, sockets), (hba, ident)) = match opened {
            Ok(opened) => opened,
            Err(error) => {
                for path in &owned {
                    let _ = std::fs::remove_file(path);
                }
                return Err(error);
            }
        };
        let (stop_read, stop) = UnixStream::pair().map_err(|e| e.to_string())?;
        let defaults = defaults(&config, &state);
        let shared = Arc::new(Shared {
            base,
            config: Mutex::new(Arc::new(config)),
            state: Mutex::new(state),
            defaults: Mutex::new(Arc::new(defaults)),
            tls: Mutex::new(tls),
            mock_nonce,
            hba: Mutex::new(Arc::new(hba)),
            ident: Mutex::new(Arc::new(ident)),
            roles,
            databases,
            open: Mutex::new(HashMap::new()),
            ddl: Mutex::new(()),
            sessions: Mutex::new(Sessions {
                next: 1001 + i32::from(poll::random::<2>()[0]) * 64,
                map: HashMap::new(),
                held: HashSet::new(),
            }),
            threads: Mutex::new(Vec::new()),
            waiting: Mutex::new(Waiting::default()),
            shutdown: AtomicU8::new(0),
        });
        let acceptor = {
            let shared = shared.clone();
            std::thread::Builder::new()
                .name("rudb-acceptor".to_owned())
                .spawn(move || accept(&shared, &listeners, &stop_read))
                .map_err(|e| e.to_string())?
        };
        log("LOG", "database system is ready to accept connections");
        Ok(Server { shared, acceptor: Some(acceptor), stop, addresses, sockets, owned })
    }

    /// Binds the TCP addresses and the Unix sockets of the configuration.
    #[allow(clippy::type_complexity)]
    fn listen(
        config: &Config,
        owned: &mut Vec<PathBuf>,
    ) -> Result<(Vec<Listener>, Vec<SocketAddr>, Vec<PathBuf>), String> {
        let mut listeners = Vec::new();
        let mut addresses = Vec::new();
        let mut port = config.port;
        let hosts: Vec<&str> =
            config.listen_addresses.split(',').map(str::trim).filter(|h| !h.is_empty()).collect();
        for host in &hosts {
            // `*` is every address. IPv6 goes first, because on Linux a socket on `::` also takes
            // IPv4 and then `0.0.0.0` is in use, which is no error.
            let candidates: Vec<SocketAddr> = if *host == "*" {
                vec![SocketAddr::from(([0u16; 8], port)), SocketAddr::from(([0u8; 4], port))]
            } else {
                match (*host, port).to_socket_addrs() {
                    Ok(found) => found.collect(),
                    Err(e) => {
                        log("WARNING", &format!("could not translate host name \"{host}\": {e}"));
                        continue;
                    }
                }
            };
            for mut address in candidates {
                address.set_port(port);
                if addresses.contains(&address) {
                    continue;
                }
                let family = if address.is_ipv4() { "IPv4" } else { "IPv6" };
                match TcpListener::bind(address) {
                    Ok(listener) => {
                        let bound = listener.local_addr().map_err(|e| e.to_string())?;
                        // With port 0 the first bind chooses the port for all the others.
                        port = bound.port();
                        listener.set_nonblocking(true).map_err(|e| e.to_string())?;
                        log(
                            "LOG",
                            &format!(
                                "listening on {family} address \"{}\", port {port}",
                                bound.ip()
                            ),
                        );
                        addresses.push(bound);
                        listeners.push(Listener::Tcp(listener));
                    }
                    Err(e) if *host == "*" && e.kind() == io::ErrorKind::AddrInUse => {}
                    Err(e) => log(
                        "WARNING",
                        &format!("could not bind {family} address \"{}\": {e}", address.ip()),
                    ),
                }
            }
        }
        if !hosts.is_empty() && addresses.is_empty() {
            return Err("could not create any TCP/IP sockets".to_owned());
        }
        let mut sockets = Vec::new();
        for dir in config.unix_socket_directories.split(',').map(str::trim) {
            if dir.is_empty() {
                continue;
            }
            let path = Path::new(dir).join(format!(".s.PGSQL.{port}"));
            let lock = Path::new(dir).join(format!(".s.PGSQL.{port}.lock"));
            lock_file(
                &lock,
                &format!("{}\n{}\n", std::process::id(), config.data.display()),
                &format!("socket \"{}\"", path.display()),
            )?;
            owned.push(lock);
            // A socket file that is left from a server that died is in the way of the bind.
            let _ = std::fs::remove_file(&path);
            let listener = UnixListener::bind(&path)
                .map_err(|e| format!("could not bind Unix address \"{}\": {e}", path.display()))?;
            owned.push(path.clone());
            std::fs::set_permissions(
                &path,
                std::fs::Permissions::from_mode(config.unix_socket_permissions),
            )
            .map_err(|e| {
                format!("could not set permissions of file \"{}\": {e}", path.display())
            })?;
            listener.set_nonblocking(true).map_err(|e| e.to_string())?;
            log("LOG", &format!("listening on Unix socket \"{}\"", path.display()));
            sockets.push(path);
            listeners.push(Listener::Unix(listener));
        }
        if listeners.is_empty() {
            return Err("no socket created for listening".to_owned());
        }
        Ok((listeners, addresses, sockets))
    }

    /// The TCP addresses that the server listens on, with the port that it uses.
    pub fn addresses(&self) -> &[SocketAddr] {
        &self.addresses
    }

    /// The Unix sockets that the server listens on.
    pub fn sockets(&self) -> &[PathBuf] {
        &self.sockets
    }

    /// The port: the port of the first TCP address, or the port of the configuration when there
    /// is no TCP address.
    pub fn port(&self) -> u16 {
        self.addresses.first().map_or(self.shared.config().port, SocketAddr::port)
    }

    /// Reads the configuration files, `pg_hba.conf`, `pg_ident.conf` and the TLS files again,
    /// as PostgreSQL does after `SIGHUP`. Each change and each error goes to the log. The
    /// sessions take the new values before the next message that they handle.
    pub fn reload(&self) {
        self.shared.reload();
    }

    /// The number of sessions that finished their startup and did not end yet.
    pub fn sessions(&self) -> usize {
        lock(&self.shared.sessions).map.values().filter(|entry| entry.key.is_some()).count()
    }

    /// Starts a shutdown in `mode` and gives at once. A smart shutdown refuses new sessions and
    /// lets the sessions run, and [`Server::finished`] tells when the last one ended. A fast or
    /// an immediate shutdown ends the sessions, and [`Server::stop`] then finishes it. A later
    /// request for a stronger mode takes over, as in PostgreSQL.
    pub fn request(&self, mode: Shutdown) {
        self.shared.request(mode);
    }

    /// True when a smart shutdown is in progress and no session is left, so that
    /// [`Server::stop`] can finish it.
    pub fn finished(&self) -> bool {
        self.shared.refusing() && lock(&self.shared.sessions).map.is_empty()
    }

    /// Stops the server. With no shutdown in progress, this is the fast shutdown of PostgreSQL:
    /// the server accepts no more connections, stops the statement of each session, ends each
    /// session with `57P01`, writes each database to its file, and removes its sockets and lock
    /// files. After [`Server::request`] with [`Shutdown::Immediate`], the server does not write
    /// the databases.
    ///
    /// # Errors
    ///
    /// The databases that the server could not write, one line for each.
    pub fn stop(mut self) -> Result<(), String> {
        self.shutdown()
    }

    fn shutdown(&mut self) -> Result<(), String> {
        let Some(acceptor) = self.acceptor.take() else {
            return Ok(());
        };
        if !self.shared.refusing() {
            self.shared.request(Shutdown::Fast);
        }
        // The rest of a smart shutdown runs as a fast one, with no session left to end.
        self.shared.shutdown.fetch_max(Shutdown::Fast as u8, Ordering::AcqRel);
        let immediate = self.shared.immediate();
        let _ = self.stop.write_all(b"x");
        let _ = acceptor.join();
        let deadline = Instant::now() + STOP_GRACE;
        while !lock(&self.shared.sessions).map.is_empty() && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(5));
        }
        for entry in lock(&self.shared.sessions).map.values() {
            if let Some(stream) = &entry.stream {
                stream.shutdown();
            }
        }
        // A thread that waits for a connection ends when its sender goes. A thread that ends its
        // session after this does not wait, because the shutdown is in progress.
        lock(&self.shared.waiting).threads.clear();
        let threads = std::mem::take(&mut *lock(&self.shared.threads));
        for thread in threads {
            let _ = thread.join();
        }
        let mut errors = Vec::new();
        let mut open = std::mem::take(&mut *lock(&self.shared.open));
        if immediate {
            // The last handle writes the file when it goes, so the handles stay, as the memory
            // of a PostgreSQL server that stops with no checkpoint.
            std::mem::forget(std::mem::take(&mut open));
        } else {
            log("LOG", "shutting down");
        }
        let catalog = self.shared.databases.snapshot();
        for (oid, database) in open {
            // Each session is gone, so this is the last handle and the close writes the file.
            if let Ok(database) = Arc::try_unwrap(database)
                && let Err(error) = database.close()
            {
                let name =
                    catalog.by_oid(oid).map_or_else(|| oid.to_string(), |row| row.name.clone());
                errors.push(format!("could not write database \"{name}\": {}", error.message()));
            }
        }
        for path in self.owned.iter().rev() {
            let _ = std::fs::remove_file(path);
        }
        log("LOG", "database system is shut down");
        if errors.is_empty() { Ok(()) } else { Err(errors.join("\n")) }
    }
}

impl Drop for Server {
    fn drop(&mut self) {
        if let Err(error) = self.shutdown() {
            log("LOG", &error);
        }
    }
}

/// The acceptor: waits on every listener and on the stop pipe, and starts a thread for each
/// connection.
fn accept(shared: &Arc<Shared>, listeners: &[Listener], stop: &UnixStream) {
    let mut fds: Vec<RawFd> = listeners.iter().map(Listener::fd).collect();
    fds.push(stop.as_raw_fd());
    loop {
        let ready = match poll::readable_any(&fds) {
            Ok(ready) => ready,
            Err(e) => {
                log("LOG", &format!("poll() failed in the acceptor: {e}"));
                std::thread::sleep(Duration::from_millis(100));
                continue;
            }
        };
        if ready[listeners.len()] || shared.stopping() {
            return;
        }
        for (listener, _) in listeners.iter().zip(&ready).filter(|(_, ready)| **ready) {
            loop {
                match listener.accept() {
                    Ok(Some(stream)) => spawn(shared, stream),
                    Ok(None) => break,
                    Err(e) => {
                        // Too many open files and the like. PostgreSQL logs it and goes on.
                        log("LOG", &format!("could not accept new connection: {e}"));
                        break;
                    }
                }
            }
        }
    }
}

/// The session threads that wait for a new connection.
#[derive(Debug, Default)]
struct Waiting {
    next: u64,
    threads: Vec<(u64, mpsc::SyncSender<Stream>)>,
}

/// Gives a connection to a thread that waits for one, or starts a thread for it.
fn spawn(shared: &Arc<Shared>, stream: Stream) {
    let mut stream = stream;
    loop {
        let Some((_, waiting)) = lock(&shared.waiting).threads.pop() else { break };
        match waiting.send(stream) {
            Ok(()) => return,
            // The thread ended at the same time.
            Err(mpsc::SendError(back)) => stream = back,
        }
    }
    let mut threads = lock(&shared.threads);
    threads.retain(|thread| !thread.is_finished());
    let session = shared.clone();
    let started = std::thread::Builder::new()
        .name("rudb-session".to_owned())
        .stack_size(SESSION_STACK)
        .spawn(move || serve(&session, stream));
    match started {
        Ok(thread) => threads.push(thread),
        Err(e) => log("LOG", &format!("could not start a session thread: {e}")),
    }
}

/// The body of a session thread: runs one session, then waits for the next connection, until
/// no connection comes in `THREAD_WAIT`, enough threads already wait, or the server stops.
fn serve(shared: &Arc<Shared>, stream: Stream) {
    let mut stream = stream;
    loop {
        session::run(shared, stream);
        let (give, take) = mpsc::sync_channel(1);
        let number = {
            let mut waiting = lock(&shared.waiting);
            // Under the lock, so that a shutdown that empties the list sees this thread in it or
            // this thread sees the shutdown.
            if shared.refusing() || waiting.threads.len() >= WAITING_THREADS {
                return;
            }
            waiting.next += 1;
            let number = waiting.next;
            waiting.threads.push((number, give));
            number
        };
        stream = match take.recv_timeout(THREAD_WAIT) {
            Ok(stream) => stream,
            Err(mpsc::RecvTimeoutError::Disconnected) => return,
            Err(mpsc::RecvTimeoutError::Timeout) => {
                let mut waiting = lock(&shared.waiting);
                if let Some(at) = waiting.threads.iter().position(|(held, _)| *held == number) {
                    waiting.threads.remove(at);
                    return;
                }
                drop(waiting);
                // The acceptor took this thread from the list, so its connection comes now.
                match take.recv() {
                    Ok(stream) => stream,
                    Err(_) => return,
                }
            }
        };
    }
}
