//! `rudb-server`: a server that speaks the PostgreSQL protocol in front of rudb databases.
//!
//! The server listens on TCP and on Unix sockets with the names and the defaults of PostgreSQL,
//! and runs each session on a thread of its own. The thread waits with `poll(2)` on the socket of
//! the client and on a wake pipe, so another thread can stop the session at any time. The codec
//! of the protocol is `rudb-pgwire`, the format of the values is `rudb-pgtypes`, and the queries
//! run in `rudb`.
//!
//! A data directory holds one rudb file for each database in `base/`. `rudb-server init` makes
//! the directory with the databases `postgres`, `template1` and `template0`, as `initdb` does.
//!
//! Document 05 of the PostgreSQL compatibility notes is the plan for this crate.

mod conf;
mod config;
mod crypto;
mod databases;
mod hba;
mod locale;
mod poll;
mod roles;
mod server;
mod session;
mod stream;
mod tls;
mod x509;

pub use config::Config;
pub use roles::os_user;
pub use server::{Init, Server, init};
