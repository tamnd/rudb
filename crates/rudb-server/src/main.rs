//! The `rudb-server` binary: `rudb-server init [-U NAME] [--pwfile FILE] DIR` makes a data
//! directory, and `rudb-server -D DIR` with the options of `postgres` runs the server until
//! `SIGINT`, `SIGTERM` or `SIGQUIT`.

use std::path::PathBuf;
use std::process::ExitCode;

use rudb_server::{Config, Server, init};

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    match args.first().map(String::as_str) {
        Some("-V" | "--version") => {
            println!("rudb-server (rudb) {}", env!("CARGO_PKG_VERSION"));
            ExitCode::SUCCESS
        }
        Some("init") => match init_args(&args[1..]) {
            Ok((data, superuser, password)) => match init(&data, &superuser, password.as_deref()) {
                Ok(()) => {
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
/// and `--pwfile` for a file with the password of the superuser on its first line.
fn init_args(args: &[String]) -> Result<(PathBuf, String, Option<String>), String> {
    let mut data = None;
    let mut superuser = None;
    let mut pwfile = None;
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
            _ if name.starts_with('-') => return Err(format!("unrecognized option: {name}")),
            _ => data = Some(PathBuf::from(arg)),
        }
    }
    let data = data
        .or_else(|| std::env::var_os("PGDATA").map(PathBuf::from))
        .ok_or_else(|| "no data directory specified".to_owned())?;
    let superuser = superuser.unwrap_or_else(rudb_server::os_user);
    if superuser.is_empty() {
        return Err("superuser name must not be empty".to_owned());
    }
    let password = match pwfile {
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
    Ok((data, superuser, password))
}

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
    let server = match Server::start(config) {
        Ok(server) => server,
        Err(error) => {
            eprintln!("FATAL:  {error}");
            return ExitCode::FAILURE;
        }
    };
    let mut signal: libc::c_int = 0;
    // SAFETY: `signals` and `signal` are valid for the whole call.
    unsafe { libc::sigwait(&raw const signals, &raw mut signal) };
    match server.stop() {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("LOG:  {error}");
            ExitCode::FAILURE
        }
    }
}

fn signal_set() -> libc::sigset_t {
    // SAFETY: `sigemptyset` sets up the whole set before `sigaddset` reads it.
    unsafe {
        let mut set = std::mem::zeroed::<libc::sigset_t>();
        libc::sigemptyset(&raw mut set);
        for signal in [libc::SIGINT, libc::SIGTERM, libc::SIGQUIT] {
            libc::sigaddset(&raw mut set, signal);
        }
        set
    }
}
