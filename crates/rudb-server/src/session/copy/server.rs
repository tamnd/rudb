//! The files and the programs of `COPY ... FROM 'file'`, `COPY ... TO 'file'` and `COPY ...
//! PROGRAM 'command'`, which the server process reads and writes itself, as `copyfrom.c` and
//! `copyto.c` do.
//!
//! The data has the same formats as on the connection, so the copy reads it with the same
//! [`super::Load`] and writes it with the same rows as `COPY TO STDOUT`. This module opens the
//! file or starts the program, checks the privileges of the role, and gives the errors of
//! PostgreSQL for each step.

use std::fs::File;
use std::io::{self, Read, Write};
use std::os::unix::fs::OpenOptionsExt;
use std::os::unix::process::{CommandExt, ExitStatusExt};
use std::path::Path;
use std::process::{Child, ChildStdin, ChildStdout, Command, ExitStatus, Stdio};

use super::{Endpoint, Failure, failure};
use crate::roles::Catalog;
use crate::tls::os_text;

/// `ROLE_PG_READ_SERVER_FILES`.
const READ_SERVER_FILES: u32 = 4569;
/// `ROLE_PG_WRITE_SERVER_FILES`.
const WRITE_SERVER_FILES: u32 = 4570;
/// `ROLE_PG_EXECUTE_SERVER_PROGRAM`.
const EXECUTE_SERVER_PROGRAM: u32 = 4571;

const HINT_CLIENT: &str =
    "Anyone can COPY to stdout or from stdin. psql's \\copy command also works for anyone.";

/// The check of `DoCopy` that the role may read or write the files of the server or run its
/// programs.
pub(super) fn privileges(
    endpoint: &Endpoint,
    from: bool,
    roles: &Catalog,
    role: u32,
) -> Result<(), Failure> {
    let (oid, name, what) = match endpoint {
        Endpoint::Client => return Ok(()),
        Endpoint::Program(_) => (
            EXECUTE_SERVER_PROGRAM,
            "pg_execute_server_program",
            "COPY to or from an external program",
        ),
        Endpoint::File(_) if from => {
            (READ_SERVER_FILES, "pg_read_server_files", "COPY from a file")
        }
        Endpoint::File(_) => (WRITE_SERVER_FILES, "pg_write_server_files", "COPY to a file"),
    };
    if roles.has_privs(role, oid) {
        return Ok(());
    }
    let mut failure =
        failure("42501", format!("permission denied to {what}")).hint(HINT_CLIENT.to_owned());
    let fields = failure.fields.get_or_insert_with(Default::default);
    fields.detail = Some(format!("Only roles with privileges of the \"{name}\" role may {what}."));
    Err(failure)
}

/// The data of `COPY FROM` a file or a program.
pub(super) enum Source {
    File(File),
    Program(Running<ChildStdout>),
}

impl Source {
    /// Opens the file, with a relative path in the data directory, or starts the program, as
    /// `BeginCopyFrom` does.
    pub(super) fn open(endpoint: &Endpoint, data: &str) -> Result<Source, Failure> {
        match endpoint {
            Endpoint::Client => unreachable!("a copy from the client has no source"),
            Endpoint::Program(command) => {
                let mut child = shell(command).stdout(Stdio::piped()).spawn().map_err(|e| {
                    file_failure(&e, format!("could not execute command \"{command}\""))
                })?;
                let pipe = child.stdout.take().expect("the pipe of the program");
                Ok(Source::Program(Running { command: command.clone(), child, pipe: Some(pipe) }))
            }
            Endpoint::File(name) => {
                let file = File::open(Path::new(data).join(name)).map_err(|e| {
                    let failure = file_failure(&e, format!("could not open file \"{name}\" for reading"));
                    if missing_or_denied(&e) {
                        let hint = "COPY FROM instructs the PostgreSQL server process to read a \
                                    file. You may want a client-side facility such as psql's \\copy.";
                        return failure.hint(hint.to_owned());
                    }
                    failure
                })?;
                let meta = file
                    .metadata()
                    .map_err(|e| file_failure(&e, format!("could not stat file \"{name}\"")))?;
                if meta.is_dir() {
                    return Err(failure("42809", format!("\"{name}\" is a directory")));
                }
                Ok(Source::File(file))
            }
        }
    }

    /// Reads the next bytes, or none at the end of the data.
    pub(super) fn read(&mut self, buf: &mut [u8]) -> Result<usize, Failure> {
        let read = match self {
            Source::File(file) => file.read(buf),
            Source::Program(running) => running.pipe.as_mut().map_or(Ok(0), |pipe| pipe.read(buf)),
        };
        read.map_err(|e| file_failure(&e, "could not read from COPY file".to_owned()))
    }

    /// Ends the copy, with the error of `ClosePipeFromProgram` for a program that failed. A
    /// program that the copy stopped reading before its end can die of `SIGPIPE`, which is not
    /// an error.
    pub(super) fn close(self, reached_end: bool) -> Result<(), Failure> {
        let Source::Program(mut running) = self else { return Ok(()) };
        let status = running.wait()?;
        if status.success() || (!reached_end && status.signal() == Some(libc::SIGPIPE)) {
            return Ok(());
        }
        Err(running.failed(status))
    }
}

/// Where the rows of `COPY TO` a file or a program go.
pub(super) enum Sink {
    File(File),
    Program(Running<ChildStdin>),
}

impl Sink {
    /// Creates the file, which must have an absolute path, or starts the program, as
    /// `BeginCopyTo` does.
    pub(super) fn open(endpoint: &Endpoint) -> Result<Sink, Failure> {
        match endpoint {
            Endpoint::Client => unreachable!("a copy to the client has no sink"),
            Endpoint::Program(command) => {
                let mut child = shell(command).stdin(Stdio::piped()).spawn().map_err(|e| {
                    file_failure(&e, format!("could not execute command \"{command}\""))
                })?;
                let pipe = child.stdin.take().expect("the pipe of the program");
                Ok(Sink::Program(Running { command: command.clone(), child, pipe: Some(pipe) }))
            }
            Endpoint::File(name) => {
                if !Path::new(name).is_absolute() {
                    return Err(failure("42602", "relative path not allowed for COPY to file"));
                }
                // The mode leaves out the write bits of the group and of others, as the umask of
                // `BeginCopyTo` does.
                let file = File::options()
                    .write(true)
                    .create(true)
                    .truncate(true)
                    .mode(0o644)
                    .open(name)
                    .map_err(|e| {
                        let failure =
                            file_failure(&e, format!("could not open file \"{name}\" for writing"));
                        if missing_or_denied(&e) {
                            let hint = "COPY TO instructs the PostgreSQL server process to write a \
                                        file. You may want a client-side facility such as psql's \
                                        \\copy.";
                            return failure.hint(hint.to_owned());
                        }
                        failure
                    })?;
                Ok(Sink::File(file))
            }
        }
    }

    /// Writes the rows in `data`.
    pub(super) fn write(&mut self, data: &[u8]) -> Result<(), Failure> {
        match self {
            Sink::File(file) => file
                .write_all(data)
                .map_err(|e| file_failure(&e, "could not write to COPY file".to_owned())),
            Sink::Program(running) => {
                let pipe = running.pipe.as_mut().expect("the pipe of the program");
                let Err(error) = pipe.write_all(data) else { return Ok(()) };
                // A program that ended early fails with its own error, when it has one.
                if error.kind() == io::ErrorKind::BrokenPipe {
                    let status = running.wait()?;
                    if !status.success() {
                        return Err(running.failed(status));
                    }
                }
                Err(file_failure(&error, "could not write to COPY program".to_owned()))
            }
        }
    }

    /// Ends the copy: closes the file, or the pipe with the error of `ClosePipeToProgram` for a
    /// program that failed.
    pub(super) fn close(self) -> Result<(), Failure> {
        let Sink::Program(mut running) = self else { return Ok(()) };
        let status = running.wait()?;
        if status.success() { Ok(()) } else { Err(running.failed(status)) }
    }
}

/// A program of a copy and its end of the pipe. Dropping it closes the pipe and waits for the
/// program, as the end of the transaction does in PostgreSQL after an error.
pub(super) struct Running<P> {
    command: String,
    child: Child,
    pipe: Option<P>,
}

impl<P> Running<P> {
    /// Closes the pipe and waits for the program.
    fn wait(&mut self) -> Result<ExitStatus, Failure> {
        self.pipe = None;
        self.child
            .wait()
            .map_err(|e| file_failure(&e, "could not close pipe to external command".to_owned()))
    }

    /// The error for a program that did not end with success.
    fn failed(&self, status: ExitStatus) -> Failure {
        let mut failure = failure("38000", format!("program \"{}\" failed", self.command));
        let fields = failure.fields.get_or_insert_with(Default::default);
        fields.detail = Some(wait_result(status));
        failure
    }
}

impl<P> Drop for Running<P> {
    fn drop(&mut self) {
        self.pipe = None;
        let _ = self.child.wait();
    }
}

/// `popen`: the command runs in `/bin/sh`. The program starts with no blocked signals, since the
/// threads of the server block the signals that `main` waits for.
fn shell(command: &str) -> Command {
    let mut shell = Command::new("/bin/sh");
    shell.arg("-c").arg(command);
    // SAFETY: the closure runs in the child between `fork` and `exec`, and it only calls
    // `sigemptyset` and `pthread_sigmask` on a set on its own stack, which is allowed there.
    unsafe {
        shell.pre_exec(|| {
            let mut set = std::mem::zeroed::<libc::sigset_t>();
            libc::sigemptyset(&raw mut set);
            libc::pthread_sigmask(libc::SIG_SETMASK, &raw const set, std::ptr::null_mut());
            Ok(())
        });
    }
    shell
}

/// `wait_result_to_str`.
fn wait_result(status: ExitStatus) -> String {
    if let Some(code) = status.code() {
        return match code {
            126 => "command not executable".to_owned(),
            127 => "command not found".to_owned(),
            _ => format!("child process exited with exit code {code}"),
        };
    }
    if let Some(signal) = status.signal() {
        // SAFETY: `strsignal` gives a string that stays valid until the next call in this
        // thread, and the text is copied before that.
        let name = unsafe { std::ffi::CStr::from_ptr(libc::strsignal(signal)) };
        return format!(
            "child process was terminated by signal {signal}: {}",
            name.to_string_lossy()
        );
    }
    format!("child process exited with unrecognized status {}", status.into_raw())
}

fn missing_or_denied(error: &io::Error) -> bool {
    matches!(error.raw_os_error(), Some(libc::ENOENT | libc::EACCES))
}

/// An error of a file operation with the text of `%m`, and the SQLSTATE of
/// `errcode_for_file_access`.
fn file_failure(error: &io::Error, message: String) -> Failure {
    let sqlstate = match error.raw_os_error() {
        Some(libc::EPERM | libc::EACCES | libc::EROFS) => "42501",
        Some(libc::ENOENT) => "58P01",
        Some(libc::EEXIST) => "58P02",
        Some(libc::ENOTDIR | libc::EISDIR | libc::ENOTEMPTY) => "42809",
        Some(libc::ENOSPC) => "53100",
        Some(libc::ENOMEM) => "53200",
        Some(libc::ENFILE | libc::EMFILE) => "53000",
        Some(libc::EIO) => "58030",
        Some(libc::ENAMETOOLONG) => "58P03",
        _ => "XX000",
    };
    failure(sqlstate, format!("{message}: {}", os_text(error)))
}
