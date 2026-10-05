//! Client authentication: the line of `pg_hba.conf` that matches the connection, and the
//! exchange of its method, as `ClientAuthentication` of PostgreSQL runs them.
//!
//! A failure sends the `FATAL` error of PostgreSQL to the client and writes the same error to the
//! log, with the reason and the line of `pg_hba.conf` in its `DETAIL`. The client never sees the
//! reason, so it cannot tell a bad password from a role that does not exist.
//!
//! `cert` and `clientcert=verify-full` compare the common name or the distinguished name of the
//! client certificate with the user, through the map of the line. `ident` on TCP asks the Ident
//! server of the client, RFC 1413, which user owns the connection.

use std::io::{self, Read, Write};
use std::net::{SocketAddr, TcpStream};
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use rudb_pgtypes::UNIX_TO_POSTGRES_USECS;
use rudb_pgwire::{
    Exchange, Level, MD5_WARNING, MD5_WARNING_DETAIL, Mode, PasswordType, ProtocolError,
    SCRAM_ITERATIONS, SCRAM_NONCE_LEN, Scram, password_message, split, verify_md5, verify_password,
};

use super::{Filled, Input, Start, Wire, terminated};
use crate::crypto::Provider;
use crate::hba::{Client, ClientCert, ClientName, HbaLine, Method, system_user_name};
use crate::poll;
use crate::roles::Catalog;
use crate::server::{Shared, log};
use crate::stream::Stream;
use crate::tls::os_text;

/// The port of the Ident server, RFC 1413.
const IDENT_PORT: u16 = 113;

/// The longest user name that an Ident server can give.
const IDENT_USERNAME_MAX: usize = 512;

/// How long the server waits for the Ident server, the default of `authentication_timeout`.
const IDENT_TIMEOUT: Duration = Duration::from_secs(60);

/// The default of `password_expiration_warning_threshold`, in seconds.
const EXPIRATION_WARNING_THRESHOLD: u64 = 7 * 24 * 60 * 60;

/// A warning that goes to the client at the end of the startup, `StoreConnectionWarning`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct Notice {
    pub(super) message: String,
    pub(super) detail: String,
    /// The warning about an MD5 secret, which the setting `md5_password_warnings` turns off.
    pub(super) md5: bool,
}

impl Notice {
    fn md5() -> Notice {
        Notice { message: MD5_WARNING.to_owned(), detail: MD5_WARNING_DETAIL.to_owned(), md5: true }
    }
}

/// What an authentication step leads to.
enum Step {
    Ok,
    /// The check failed, with the reason for the log.
    Failed(Option<String>),
    /// The connection ended: the client closed it, or an error already went out.
    Ended,
}

/// Runs the authentication of a new connection. `Some` holds the warnings for the end of the
/// startup, and then the caller sends `AuthenticationOk`. `None` means that the connection ends,
/// and the error, if any, already went out.
pub(super) fn authenticate(
    shared: &Shared,
    start: &Start,
    wire: &mut Wire,
    input: &mut Input,
) -> io::Result<Option<Vec<Notice>>> {
    let address = match &wire.stream {
        Stream::Tcp(socket) => Some(socket.peer_addr()?.ip().to_canonical()),
        Stream::Tls(stream) => Some(stream.sock.peer_addr()?.ip().to_canonical()),
        Stream::Unix(_) => None,
    };
    let ssl = matches!(wire.stream, Stream::Tls(_));
    let catalog = shared.roles.snapshot();
    let hba = shared.hba();
    let client = Client::new(address, ssl, &start.user, &start.database);
    let encryption = if ssl { "SSL encryption" } else { "no encryption" };
    let Some(line) = hba.find(&client, &catalog) else {
        let message = format!(
            "no pg_hba.conf entry for host \"{}\", user \"{}\", database \"{}\", {encryption}",
            client.host(),
            start.user,
            start.database
        );
        wire.fatal_with(("28000", message), client.lookup_detail().as_deref())?;
        return Ok(None);
    };
    if line.clientcert != ClientCert::Off {
        if !wire.tls.as_ref().is_some_and(|tls| tls.ca) {
            let message = "client certificates can only be checked if a root certificate store \
                           is available";
            wire.fatal(("F0000", message.to_owned()))?;
            return Ok(None);
        }
        if wire.peer.is_none() {
            let message = "connection requires a valid client certificate";
            wire.fatal(("28000", message.to_owned()))?;
            return Ok(None);
        }
    }
    if line.method == Method::Reject {
        let message = format!(
            "pg_hba.conf rejects connection for host \"{}\", user \"{}\", database \"{}\", \
             {encryption}",
            client.host(),
            start.user,
            start.database
        );
        wire.fatal(("28000", message))?;
        return Ok(None);
    }
    let mut notices = Vec::new();
    let mut run = Run { shared, start, wire, input, catalog: &catalog };
    let step = match line.method {
        // `cert` is `trust` with the check of the certificate below.
        Method::Trust | Method::Cert => Step::Ok,
        Method::Peer => run.peer(line),
        Method::Ident => run.ident(line),
        Method::Password => run.password(&mut notices)?,
        Method::Md5 | Method::Scram => run.challenge(line.method, ssl, &mut notices)?,
        Method::OAuth => {
            log(
                "LOG",
                &format!(
                    "authentication method \"{}\" is not supported for this connection by this \
                     version of rudb-server",
                    method_name(line.method)
                ),
            );
            Step::Failed(None)
        }
        Method::Reject => unreachable!("handled above"),
    };
    let step = match step {
        Step::Ok if line.clientcert == ClientCert::VerifyFull || line.method == Method::Cert => {
            run.cert(line)
        }
        step => step,
    };
    match step {
        Step::Ok => Ok(Some(notices)),
        Step::Ended => Ok(None),
        Step::Failed(detail) => {
            failed(run.wire, line, &start.user, detail)?;
            Ok(None)
        }
    }
}

/// The name of a method in `pg_hba.conf`.
fn method_name(method: Method) -> &'static str {
    match method {
        Method::Trust => "trust",
        Method::Reject => "reject",
        Method::Ident => "ident",
        Method::Peer => "peer",
        Method::Password => "password",
        Method::Md5 => "md5",
        Method::Scram => "scram-sha-256",
        Method::Cert => "cert",
        Method::OAuth => "oauth",
    }
}

/// `auth_failed`: the error of the method to the client, and to the log with the reason and the
/// line of `pg_hba.conf`.
fn failed(wire: &mut Wire, line: &HbaLine, user: &str, detail: Option<String>) -> io::Result<()> {
    let (sqlstate, message) = line.method.failed(user);
    let matched =
        format!("Connection matched file \"{}\" line {}: \"{}\"", line.file, line.number, line.raw);
    let detail = match detail {
        Some(detail) => format!("{detail}\n{matched}"),
        None => matched,
    };
    wire.fatal_with((sqlstate, message), Some(&detail))
}

/// The state of one authentication.
struct Run<'a> {
    shared: &'a Shared,
    start: &'a Start,
    wire: &'a mut Wire,
    input: &'a mut Input,
    catalog: &'a Catalog,
}

impl Run<'_> {
    /// Sends the output and reads one message of the exchange. `None` means that the
    /// connection ended.
    fn read(&mut self, mode: Mode) -> io::Result<Option<Vec<u8>>> {
        self.wire.flush()?;
        loop {
            match split(self.input.pending(), mode) {
                Ok(Some(frame)) => {
                    let body = frame.body.to_vec();
                    let size = frame.size();
                    self.input.consume(size);
                    return Ok(Some(body));
                }
                Ok(None) => match self.wire.fill(self.input)? {
                    Filled::Data => {}
                    Filled::Closed => return Ok(None),
                    Filled::Woken if self.shared.stopping() => {
                        terminated(self.wire)?;
                        return Ok(None);
                    }
                    Filled::Woken => {}
                },
                Err(error) => {
                    self.error(&error)?;
                    return Ok(None);
                }
            }
        }
    }

    /// Sends an error of the protocol, or writes it to the log when it is for the log only.
    fn error(&mut self, error: &ProtocolError) -> io::Result<()> {
        if error.level == Level::Log {
            log("LOG", &error.message);
            return Ok(());
        }
        match &error.detail {
            Some(detail) => log("FATAL", &format!("{}\nDETAIL:  {detail}", error.message)),
            None => log("FATAL", &error.message),
        }
        self.wire.out.protocol_error(error, self.start.protocol);
        self.wire.flush()
    }

    /// Reads the password of a `PasswordMessage`. `None` means that the connection ended.
    fn password_message(&mut self) -> io::Result<Option<Vec<u8>>> {
        let Some(body) = self.read(Mode::Password)? else {
            return Ok(None);
        };
        match password_message(&body) {
            Ok(password) => Ok(Some(password.to_vec())),
            Err(error) => {
                self.error(&error)?;
                Ok(None)
            }
        }
    }

    /// `CheckPasswordAuth`: the `password` method, with the password in clear text.
    fn password(&mut self, notices: &mut Vec<Notice>) -> io::Result<Step> {
        self.wire.out.authentication_cleartext_password();
        let Some(password) = self.password_message()? else {
            return Ok(Step::Ended);
        };
        let user = self.start.user.as_str();
        let secret = match role_password(self.catalog, user, notices) {
            Ok(secret) => secret,
            Err(detail) => return Ok(Step::Failed(Some(detail))),
        };
        if verify_password(&Provider, user.as_bytes(), secret.as_bytes(), &password) {
            if PasswordType::of(secret.as_bytes()) == PasswordType::Md5 {
                notices.push(Notice::md5());
            }
            return Ok(Step::Ok);
        }
        Ok(Step::Failed(Some(match PasswordType::of(secret.as_bytes()) {
            PasswordType::Plaintext => {
                format!("Password of user \"{user}\" is in unrecognized format.")
            }
            _ => format!("Password does not match for user \"{user}\"."),
        })))
    }

    /// `CheckPWChallengeAuth`: `md5` with an MD5 secret, and SCRAM in every other case. A role
    /// that does not exist or has no valid secret goes through SCRAM with a mock secret, so the
    /// client cannot tell.
    fn challenge(
        &mut self,
        method: Method,
        ssl: bool,
        notices: &mut Vec<Notice>,
    ) -> io::Result<Step> {
        let user = self.start.user.clone();
        let (secret, mut detail) = match role_password(self.catalog, &user, notices) {
            Ok(secret) => (Some(secret), None),
            Err(detail) => (None, Some(detail)),
        };
        // A role with no secret looks like a role with a secret of `password_encryption`.
        let kind = secret.map_or(PasswordType::ScramSha256, |s| PasswordType::of(s.as_bytes()));
        if method == Method::Md5 && kind == PasswordType::Md5 {
            let secret = secret.unwrap_or_default();
            let salt = poll::random::<4>();
            self.wire.out.authentication_md5_password(salt);
            let Some(response) = self.password_message()? else {
                return Ok(Step::Ended);
            };
            if verify_md5(&Provider, secret.as_bytes(), &response, salt) {
                notices.push(Notice::md5());
                return Ok(Step::Ok);
            }
            return Ok(Step::Failed(Some(format!("Password does not match for user \"{user}\"."))));
        }
        if secret.is_some() && kind != PasswordType::ScramSha256 {
            detail = Some(format!("User \"{user}\" does not have a valid SCRAM secret."));
        }
        self.wire.out.authentication_sasl(Scram::mechanisms(ssl));
        let Some(body) = self.read(Mode::Sasl)? else {
            return Ok(Step::Ended);
        };
        let secret = Scram::secret_for(
            &Provider,
            user.as_bytes(),
            secret.map(str::as_bytes),
            &self.shared.mock_nonce,
            SCRAM_ITERATIONS,
        );
        let tls = self.wire.tls.clone();
        let hash = tls.as_ref().map(|tls| tls.hash.as_slice()).filter(|_| ssl);
        let nonce = poll::random::<SCRAM_NONCE_LEN>();
        let (mut scram, mut exchange) =
            match Scram::start(&Provider, &body, hash, secret, nonce, &mut self.wire.out) {
                Ok(started) => started,
                Err(error) => {
                    self.error(&error)?;
                    return Ok(Step::Ended);
                }
            };
        loop {
            match exchange {
                // `AuthenticationSASLFinal` goes out with `AuthenticationOk`.
                Exchange::Success => return Ok(Step::Ok),
                Exchange::Failure => return Ok(Step::Failed(detail)),
                Exchange::Continue => {}
            }
            let Some(body) = self.read(Mode::Sasl)? else {
                return Ok(Step::Ended);
            };
            exchange = match scram.next(&Provider, &body, &mut self.wire.out) {
                Ok(exchange) => exchange,
                Err(error) => {
                    self.error(&error)?;
                    return Ok(Step::Ended);
                }
            };
        }
    }

    /// `CheckCertAuth`: the common name or the distinguished name of the client certificate,
    /// through the map of the line.
    fn cert(&mut self, line: &HbaLine) -> Step {
        let user = &self.start.user;
        let name = self.wire.peer.as_ref().and_then(|peer| match line.clientname {
            ClientName::Cn => peer.cn.as_deref(),
            ClientName::Dn => Some(peer.dn.as_str()),
        });
        let Some(name) = name.filter(|name| !name.is_empty()) else {
            log(
                "LOG",
                &format!(
                    "certificate authentication failed for user \"{user}\": client certificate \
                     contains no user name"
                ),
            );
            return Step::Failed(None);
        };
        let ident = self.shared.ident();
        if ident.check(line.map.as_deref(), user, name, self.catalog) {
            return Step::Ok;
        }
        if line.clientcert == ClientCert::VerifyFull && line.method != Method::Cert {
            let field = match line.clientname {
                ClientName::Cn => "CN",
                ClientName::Dn => "DN",
            };
            log(
                "LOG",
                &format!(
                    "certificate validation (clientcert=verify-full) failed for user \"{user}\": \
                     {field} mismatch"
                ),
            );
        }
        Step::Failed(None)
    }

    /// `ident_inet`: the user that the Ident server of the client gives for the connection,
    /// through the map of the line.
    fn ident(&mut self, line: &HbaLine) -> Step {
        let addresses = match &self.wire.stream {
            Stream::Tcp(socket) => socket.local_addr().and_then(|l| Ok((l, socket.peer_addr()?))),
            Stream::Tls(stream) => {
                stream.sock.local_addr().and_then(|l| Ok((l, stream.sock.peer_addr()?)))
            }
            Stream::Unix(_) => return Step::Failed(None),
        };
        let Ok((local, remote)) = addresses else {
            return Step::Failed(None);
        };
        let name = ident_query(local, remote, IDENT_PORT).and_then(|response| {
            ident_user(&response).ok_or_else(|| {
                let response = response.split(|&byte| byte == 0).next().unwrap_or_default();
                format!(
                    "invalidly formatted response from Ident server: \"{}\"",
                    String::from_utf8_lossy(response)
                )
            })
        });
        let name = match name {
            Ok(name) => name,
            Err(message) => {
                log("LOG", &message);
                return Step::Failed(None);
            }
        };
        let ident = self.shared.ident();
        if ident.check(line.map.as_deref(), &self.start.user, &name, self.catalog) {
            Step::Ok
        } else {
            Step::Failed(None)
        }
    }

    /// `auth_peer`: the system user of the other end of the Unix socket, through the map of the
    /// line.
    fn peer(&mut self, line: &HbaLine) -> Step {
        let Stream::Unix(socket) = &self.wire.stream else {
            return Step::Failed(None);
        };
        let uid = match peer_uid(socket.as_raw_fd()) {
            Ok(uid) => uid,
            Err(error) => {
                log("LOG", &format!("could not get peer credentials: {}", os_text(&error)));
                return Step::Failed(None);
            }
        };
        let Some(name) = system_user_name(uid) else {
            log("LOG", &format!("could not look up local user ID {uid}: user does not exist"));
            return Step::Failed(None);
        };
        let ident = self.shared.ident();
        if ident.check(line.map.as_deref(), &self.start.user, &name, self.catalog) {
            Step::Ok
        } else {
            Step::Failed(None)
        }
    }
}

/// Asks the Ident server at `port` of the client which user owns the connection from `remote` to
/// `local`, and gives the answer. The socket is bound to the local address of the connection, so
/// that the Ident server finds the connection when the server has more than one address. An
/// error is the text of the log line of PostgreSQL.
fn ident_query(local: SocketAddr, remote: SocketAddr, port: u16) -> Result<Vec<u8>, String> {
    let (remote_host, local_host) = (remote.ip().to_string(), local.ip().to_string());
    let failed = |what: &str, e: &io::Error| {
        format!(
            "could not {what} Ident server at address \"{remote_host}\", port {port}: {}",
            os_text(e)
        )
    };
    let family = match remote {
        SocketAddr::V4(_) => libc::AF_INET,
        SocketAddr::V6(_) => libc::AF_INET6,
    };
    #[cfg(any(target_os = "linux", target_os = "android"))]
    let kind = libc::SOCK_STREAM | libc::SOCK_CLOEXEC;
    #[cfg(not(any(target_os = "linux", target_os = "android")))]
    let kind = libc::SOCK_STREAM;
    // SAFETY: `socket` takes no pointers.
    let fd = unsafe { libc::socket(family, kind, 0) };
    if fd < 0 {
        let e = io::Error::last_os_error();
        return Err(format!("could not create socket for Ident connection: {}", os_text(&e)));
    }
    // SAFETY: `fd` is a new socket that nothing else owns.
    let socket = unsafe { OwnedFd::from_raw_fd(fd) };
    let (address, len) = sockaddr(SocketAddr::new(local.ip(), 0));
    // SAFETY: `address` is a valid socket address of `len` bytes.
    if unsafe { libc::bind(socket.as_raw_fd(), (&raw const address).cast(), len) } != 0 {
        let e = io::Error::last_os_error();
        return Err(format!("could not bind to local address \"{local_host}\": {}", os_text(&e)));
    }
    let (address, len) = sockaddr(SocketAddr::new(remote.ip(), port));
    // SAFETY: `address` is a valid socket address of `len` bytes.
    if unsafe { libc::connect(socket.as_raw_fd(), (&raw const address).cast(), len) } != 0 {
        return Err(failed("connect to", &io::Error::last_os_error()));
    }
    let mut stream = TcpStream::from(socket);
    let _ = stream.set_read_timeout(Some(IDENT_TIMEOUT));
    let _ = stream.set_write_timeout(Some(IDENT_TIMEOUT));
    let query = format!("{},{}\r\n", remote.port(), local.port());
    stream.write_all(query.as_bytes()).map_err(|e| failed("send query to", &e))?;
    let mut response = vec![0; 80 + IDENT_USERNAME_MAX - 1];
    let n = loop {
        match stream.read(&mut response) {
            Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
            read => break read.map_err(|e| failed("receive response from", &e))?,
        }
    };
    response.truncate(n);
    Ok(response)
}

/// `interpret_ident_response`: the user name of a `USERID` answer of an Ident server, which ends
/// with a carriage return and a line feed. Space and tab are the white space of RFC 1413.
fn ident_user(response: &[u8]) -> Option<String> {
    // The answer is a C string in PostgreSQL, so it ends at a zero byte.
    let response = response.split(|&byte| byte == 0).next().unwrap_or_default();
    if response.len() < 2 || response[response.len() - 2] != b'\r' {
        return None;
    }
    // Each scan below stops at the carriage return, so it stays in the answer.
    let blank = |byte: u8| byte == b' ' || byte == b'\t';
    let mut at = 0;
    let skip_to_colon = |mut at: usize| {
        while response[at] != b':' && response[at] != b'\r' {
            at += 1;
        }
        (response[at] == b':').then_some(at + 1)
    };
    // The port field.
    at = skip_to_colon(at)?;
    while blank(response[at]) {
        at += 1;
    }
    let start = at;
    while response[at] != b':' && response[at] != b'\r' && !blank(response[at]) && at - start < 79 {
        at += 1;
    }
    let kind = &response[start..at];
    while blank(response[at]) {
        at += 1;
    }
    if kind != b"USERID" || response[at] != b':' {
        return None;
    }
    // The operating system field.
    at = skip_to_colon(at + 1)?;
    while blank(response[at]) {
        at += 1;
    }
    let start = at;
    while response[at] != b'\r' && at - start < IDENT_USERNAME_MAX {
        at += 1;
    }
    Some(String::from_utf8_lossy(&response[start..at]).into_owned())
}

/// The socket address of `address` for `bind` and `connect`.
fn sockaddr(address: SocketAddr) -> (libc::sockaddr_storage, libc::socklen_t) {
    // SAFETY: a socket address of zero bytes is valid, and the fields are set below.
    let mut storage = unsafe { std::mem::zeroed::<libc::sockaddr_storage>() };
    let len = match address {
        SocketAddr::V4(v4) => {
            // SAFETY: `sockaddr_storage` is large enough and aligned for every socket address.
            let sin = unsafe { &mut *(&raw mut storage).cast::<libc::sockaddr_in>() };
            sin.sin_family = libc::AF_INET as libc::sa_family_t;
            sin.sin_port = v4.port().to_be();
            sin.sin_addr.s_addr = u32::from(*v4.ip()).to_be();
            #[cfg(any(target_os = "macos", target_os = "freebsd"))]
            {
                sin.sin_len = size_of::<libc::sockaddr_in>() as u8;
            }
            size_of::<libc::sockaddr_in>()
        }
        SocketAddr::V6(v6) => {
            // SAFETY: `sockaddr_storage` is large enough and aligned for every socket address.
            let sin6 = unsafe { &mut *(&raw mut storage).cast::<libc::sockaddr_in6>() };
            sin6.sin6_family = libc::AF_INET6 as libc::sa_family_t;
            sin6.sin6_port = v6.port().to_be();
            sin6.sin6_addr.s6_addr = v6.ip().octets();
            sin6.sin6_scope_id = v6.scope_id();
            #[cfg(any(target_os = "macos", target_os = "freebsd"))]
            {
                sin6.sin6_len = size_of::<libc::sockaddr_in6>() as u8;
            }
            size_of::<libc::sockaddr_in6>()
        }
    };
    (storage, len as libc::socklen_t)
}

/// `get_role_password`: the secret of the role, or the reason for the log why there is none. A
/// password that ends within `password_expiration_warning_threshold` adds a warning.
fn role_password<'a>(
    catalog: &'a Catalog,
    user: &str,
    notices: &mut Vec<Notice>,
) -> Result<&'a str, String> {
    let Some(role) = catalog.find(user) else {
        return Err(format!("Role \"{user}\" does not exist."));
    };
    let Some(secret) = role.password.as_deref() else {
        return Err(format!("User \"{user}\" has no password assigned."));
    };
    if let Some(until) = role.valid_until {
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_or(0, |d| i64::try_from(d.as_micros()).unwrap_or(i64::MAX))
            + UNIX_TO_POSTGRES_USECS;
        if until < now {
            return Err(format!("User \"{user}\" has an expired password."));
        }
        let left = until.abs_diff(now);
        if left / 1_000_000 < EXPIRATION_WARNING_THRESHOLD {
            notices.push(Notice {
                message: "role password will expire soon".to_owned(),
                detail: expiry_detail(user, left),
                md5: false,
            });
        }
    }
    Ok(secret)
}

/// The detail of the warning for a password that ends soon, `left` microseconds from now.
fn expiry_detail(user: &str, left: u64) -> String {
    const MINUTE: u64 = 60_000_000;
    let (days, hours, minutes) = (
        left / (1440 * MINUTE),
        left % (1440 * MINUTE) / (60 * MINUTE),
        left % (60 * MINUTE) / MINUTE,
    );
    let (count, unit) = if days > 0 {
        (days, "day")
    } else if hours > 0 {
        (hours, "hour")
    } else if minutes > 0 {
        (minutes, "minute")
    } else {
        return format!("The password for role \"{user}\" will expire in less than 1 minute.");
    };
    let plural = if count == 1 { "" } else { "s" };
    format!("The password for role \"{user}\" will expire in {count} {unit}{plural}.")
}

/// The user ID of the process at the other end of a Unix socket.
#[cfg(any(
    target_os = "macos",
    target_os = "freebsd",
    target_os = "openbsd",
    target_os = "netbsd"
))]
fn peer_uid(fd: RawFd) -> io::Result<libc::uid_t> {
    let (mut uid, mut gid) = (0, 0);
    // SAFETY: `uid` and `gid` are valid places for the IDs for the whole call.
    if unsafe { libc::getpeereid(fd, &raw mut uid, &raw mut gid) } != 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(uid)
}

/// The user ID of the process at the other end of a Unix socket.
#[cfg(any(target_os = "linux", target_os = "android"))]
fn peer_uid(fd: RawFd) -> io::Result<libc::uid_t> {
    // SAFETY: `cred` is valid for its length for the whole call.
    unsafe {
        let mut cred = std::mem::zeroed::<libc::ucred>();
        let mut len = size_of::<libc::ucred>() as libc::socklen_t;
        if libc::getsockopt(
            fd,
            libc::SOL_SOCKET,
            libc::SO_PEERCRED,
            (&raw mut cred).cast(),
            &raw mut len,
        ) != 0
        {
            return Err(io::Error::last_os_error());
        }
        Ok(cred.uid)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_answers_of_an_ident_server() {
        let user = |text: &str| ident_user(text.as_bytes());
        assert_eq!(user("6191, 23 : USERID : UNIX : stjohns\r\n").as_deref(), Some("stjohns"));
        assert_eq!(user("6191,23:USERID:OTHER:a b \r\n").as_deref(), Some("a b "));
        assert_eq!(user("6191, 23 : ERROR : NO-USER\r\n"), None);
        assert_eq!(user("6191, 23 : USERID : UNIX : x\n"), None);
        assert_eq!(user("6191, 23 : USERID\r\n"), None);
        assert_eq!(user("\r\n"), None);
        assert_eq!(user("1,2:USERID:UNIX:\r\n").as_deref(), Some(""));
        assert_eq!(user("1,2:USERID:UNIX:a\0b\r\n"), None);
    }

    #[test]
    fn a_query_to_an_ident_server() {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let server = std::thread::spawn(move || {
            let (mut socket, _) = listener.accept().unwrap();
            let mut query = [0u8; 64];
            let n = socket.read(&mut query).unwrap();
            socket.write_all(b"5432 , 40000 : USERID : UNIX : alice\r\n").unwrap();
            String::from_utf8(query[..n].to_vec()).unwrap()
        });
        let local: SocketAddr = "127.0.0.1:40000".parse().unwrap();
        let remote: SocketAddr = "127.0.0.1:5432".parse().unwrap();
        let response = ident_query(local, remote, port).unwrap();
        assert_eq!(ident_user(&response).as_deref(), Some("alice"));
        assert_eq!(server.join().unwrap(), "5432,40000\r\n");
        let error = ident_query(local, remote, 1).unwrap_err();
        assert_eq!(
            error,
            "could not connect to Ident server at address \"127.0.0.1\", port 1: Connection refused"
        );
    }

    #[test]
    fn the_detail_of_an_expiry() {
        let minute = 60_000_000;
        assert_eq!(
            expiry_detail("a", 3 * 1440 * minute + 5),
            "The password for role \"a\" will expire in 3 days."
        );
        assert_eq!(
            expiry_detail("a", 1440 * minute),
            "The password for role \"a\" will expire in 1 day."
        );
        assert_eq!(
            expiry_detail("a", 61 * minute),
            "The password for role \"a\" will expire in 1 hour."
        );
        assert_eq!(
            expiry_detail("a", 2 * minute),
            "The password for role \"a\" will expire in 2 minutes."
        );
        assert_eq!(
            expiry_detail("a", 59_000_000),
            "The password for role \"a\" will expire in less than 1 minute."
        );
    }
}
