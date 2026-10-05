//! Client authentication: the line of `pg_hba.conf` that matches the connection, and the
//! exchange of its method, as `ClientAuthentication` of PostgreSQL runs them.
//!
//! A failure sends the `FATAL` error of PostgreSQL to the client and writes the same error to the
//! log, with the reason and the line of `pg_hba.conf` in its `DETAIL`. The client never sees the
//! reason, so it cannot tell a bad password from a role that does not exist.
//!
//! `cert`, the `clientcert=verify-full` option and `ident` on TCP come in a later version. A line
//! with one of them loads, but a connection that it matches fails and the log tells why.

use std::io;
use std::os::fd::{AsRawFd, RawFd};
use std::time::{SystemTime, UNIX_EPOCH};

use rudb_pgtypes::UNIX_TO_POSTGRES_USECS;
use rudb_pgwire::{
    Exchange, Level, MD5_WARNING, MD5_WARNING_DETAIL, Mode, PasswordType, ProtocolError,
    SCRAM_ITERATIONS, SCRAM_NONCE_LEN, Scram, password_message, split, verify_md5, verify_password,
};

use super::{Filled, Input, Start, Wire, terminated};
use crate::crypto::Provider;
use crate::hba::{Client, ClientCert, HbaLine, Method, system_user_name};
use crate::poll;
use crate::roles::Catalog;
use crate::server::{Shared, log};
use crate::stream::Stream;
use crate::tls::os_text;

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
        if shared.config.ssl_ca_file.as_os_str().is_empty() {
            let message = "client certificates can only be checked if a root certificate store \
                           is available";
            wire.fatal(("F0000", message.to_owned()))?;
            return Ok(None);
        }
        let certified = match &wire.stream {
            Stream::Tls(stream) => stream.conn.peer_certificates().is_some_and(|c| !c.is_empty()),
            _ => false,
        };
        if !certified {
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
        Method::Trust => Step::Ok,
        Method::Peer => run.peer(line),
        Method::Password => run.password(&mut notices)?,
        Method::Md5 | Method::Scram => run.challenge(line.method, ssl, &mut notices)?,
        Method::Ident | Method::Cert | Method::OAuth => {
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
        Step::Ok if line.clientcert == ClientCert::VerifyFull => {
            log(
                "LOG",
                "certificate validation (clientcert=verify-full) is not supported by this \
                 version of rudb-server",
            );
            Step::Failed(None)
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
        let hash = self.shared.certificate_hash.as_deref().filter(|_| ssl);
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
