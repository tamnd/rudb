//! The `rudb-server` binary: `rudb-server init [-U NAME] [--pwfile FILE] [-A METHOD] DIR` makes a data
//! directory, and `rudb-server -D DIR` with the options of `postgres` runs the server. As in
//! PostgreSQL, `SIGTERM` starts a smart shutdown, `SIGINT` a fast one and `SIGQUIT` an immediate
//! one. `SIGHUP` reloads the configuration files.

#[cfg(unix)]
use std::path::PathBuf;
#[cfg(unix)]
use std::process::ExitCode;
#[cfg(unix)]
use std::sync::mpsc::{self, RecvTimeoutError};
#[cfg(unix)]
use std::time::Duration;

#[cfg(unix)]
use rudb_server::{Config, Init, Server, Shutdown, init};

/// The server waits with `poll(2)` and listens on Unix sockets, so it is built for Unix only. On
/// any other system the binary says so and stops.
#[cfg(not(unix))]
fn main() -> std::process::ExitCode {
    eprintln!("rudb-server: runs only on Unix");
    std::process::ExitCode::FAILURE
}

#[cfg(unix)]
fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    match args.first().map(String::as_str) {
        Some("-V" | "--version") => {
            println!("rudb-server (rudb) {}", env!("CARGO_PKG_VERSION"));
            ExitCode::SUCCESS
        }
        Some("init") => match init_args(&args[1..]) {
            Ok((data, options)) => match init(&data, &options) {
                Ok(()) => {
                    if options.trust_warning() {
                        eprintln!(
                            "\nrudb-server: warning: enabling \"trust\" authentication for local \
                             connections\nrudb-server: hint: You can change this by editing \
                             pg_hba.conf or using the option -A, or --auth-local and --auth-host, \
                             the next time you run rudb-server init."
                        );
                    }
                    println!(
                        "Success. You can now start the database server using:\n\n    rudb-server -D {}\n",
                        data.display()
                    );
                    ExitCode::SUCCESS
                }
                Err(error) => {
                    eprintln!("rudb-server: error: {error}");
                    ExitCode::FAILURE
                }
            },
            Err(error) => {
                eprintln!("rudb-server: error: {error}");
                ExitCode::FAILURE
            }
        },
        _ => run(&args),
    }
}

/// The options of `init`, which are the options of `initdb` that it knows: `-D` or `--pgdata` or
/// the last argument for the data directory, `-U` or `--username` for the name of the superuser,
/// `--pwfile` for a file with the password of the superuser on its first line, and `-A` or
/// `--auth`, `--auth-local` and `--auth-host` for the methods in `pg_hba.conf`.
#[cfg(unix)]
fn init_args(args: &[String]) -> Result<(PathBuf, Init), String> {
    let mut data = None;
    let mut superuser = None;
    let mut pwfile = None;
    let mut options = Init::default();
    let mut at = 0;
    while let Some(arg) = args.get(at) {
        at += 1;
        let (name, inline) = match arg.split_once('=') {
            Some((name, value)) if name.starts_with("--") => (name, Some(value.to_owned())),
            _ => (arg.as_str(), None),
        };
        let mut value = || {
            inline.clone().or_else(|| {
                at += 1;
                args.get(at - 1).cloned()
            })
        };
        let missing =
            || format!("option requires an argument -- '{}'", name.trim_start_matches('-'));
        match name {
            "-D" | "--pgdata" => data = Some(PathBuf::from(value().ok_or_else(missing)?)),
            "-U" | "--username" => superuser = Some(value().ok_or_else(missing)?),
            "--pwfile" => pwfile = Some(value().ok_or_else(missing)?),
            "-A" | "--auth" => options.auth(&value().ok_or_else(missing)?),
            "--auth-local" => options.auth_local = Some(value().ok_or_else(missing)?),
            "--auth-host" => options.auth_host = Some(value().ok_or_else(missing)?),
            _ if name.starts_with('-') => return Err(format!("unrecognized option: {name}")),
            _ => data = Some(PathBuf::from(arg)),
        }
    }
    let data = data
        .or_else(|| std::env::var_os("PGDATA").map(PathBuf::from))
        .ok_or_else(|| "no data directory specified".to_owned())?;
    options.superuser = superuser.unwrap_or_else(rudb_server::os_user);
    if options.superuser.is_empty() {
        return Err("superuser name must not be empty".to_owned());
    }
    options.password = match pwfile {
        Some(file) => {
            let text = std::fs::read_to_string(&file)
                .map_err(|e| format!("could not open file \"{file}\" for reading: {e}"))?;
            let line = text.lines().next().unwrap_or_default();
            if line.is_empty() {
                return Err(format!("password file \"{file}\" is empty"));
            }
            Some(line.to_owned())
        }
        None => None,
    };
    Ok((data, options))
}

#[cfg(unix)]
fn run(args: &[String]) -> ExitCode {
    let config = match Config::from_args(args) {
        Ok(config) => config,
        Err(error) => {
            eprintln!("rudb-server: {error}");
            return ExitCode::FAILURE;
        }
    };
    // The signals are blocked in every thread, so that only `sigwait` below gets them. The
    // threads that the server starts keep the mask of this thread.
    let signals = signal_set();
    // SAFETY: `signals` is a valid set, and a null old set is allowed.
    unsafe { libc::pthread_sigmask(libc::SIG_BLOCK, &raw const signals, std::ptr::null_mut()) };
    // A shell starts a background job with `SIGINT` and `SIGQUIT` ignored, and the system drops
    // an ignored signal before `sigwait` can get it. PostgreSQL sets its own handlers, so it gets
    // them all the same.
    for signal in [libc::SIGHUP, libc::SIGINT, libc::SIGTERM, libc::SIGQUIT] {
        // SAFETY: the default action is valid for each of these signals, and they are blocked.
        unsafe { libc::signal(signal, libc::SIG_DFL) };
    }
    let server = match Server::start(config) {
        Ok(server) => server,
        // PostgreSQL checks the configuration file before its log starts, so that error has the
        // name of the program and not a level.
        Err(error) if error.starts_with("could not access the server configuration file") => {
            eprintln!("rudb-server: {error}");
            return ExitCode::FAILURE;
        }
        Err(error) => {
            eprintln!("FATAL:  {error}");
            return ExitCode::FAILURE;
        }
    };
    // A thread waits for the signals, so that this thread can also see the end of a smart
    // shutdown.
    let (send, signaled) = mpsc::channel();
    std::thread::spawn(move || {
        loop {
            let mut signal: libc::c_int = 0;
            // SAFETY: `signals` and `signal` are valid for the whole call.
            unsafe { libc::sigwait(&raw const signals, &raw mut signal) };
            if send.send(signal).is_err() {
                break;
            }
        }
    });
    loop {
        match signaled.recv_timeout(Duration::from_millis(100)) {
            Ok(libc::SIGHUP) => server.reload(),
            Ok(libc::SIGTERM) => server.request(Shutdown::Smart),
            Ok(libc::SIGINT) => {
                server.request(Shutdown::Fast);
                break;
            }
            Ok(libc::SIGQUIT) => {
                server.request(Shutdown::Immediate);
                break;
            }
            Ok(_) => {}
            Err(RecvTimeoutError::Timeout) if server.finished() => break,
            Err(RecvTimeoutError::Timeout) => {}
            Err(RecvTimeoutError::Disconnected) => break,
        }
    }
    match server.stop() {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("LOG:  {error}");
            ExitCode::FAILURE
        }
    }
}

#[cfg(unix)]
fn signal_set() -> libc::sigset_t {
    // SAFETY: `sigemptyset` sets up the whole set before `sigaddset` reads it.
    unsafe {
        let mut set = std::mem::zeroed::<libc::sigset_t>();
        libc::sigemptyset(&raw mut set);
        for signal in [libc::SIGHUP, libc::SIGINT, libc::SIGTERM, libc::SIGQUIT] {
            libc::sigaddset(&raw mut set, signal);
        }
        set
    }
}
