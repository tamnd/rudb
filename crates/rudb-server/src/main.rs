//! The `rudb-server` binary: `rudb-server init DIR` makes a data directory, and `rudb-server -D
//! DIR` with the options of `postgres` runs the server until `SIGINT`, `SIGTERM` or `SIGQUIT`.

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
        Some("init") => {
            let data = args
                .get(1)
                .map(PathBuf::from)
                .or_else(|| std::env::var_os("PGDATA").map(PathBuf::from));
            let Some(data) = data else {
                eprintln!("rudb-server: no data directory specified");
                return ExitCode::FAILURE;
            };
            match init(&data) {
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
            }
        }
        _ => run(&args),
    }
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
