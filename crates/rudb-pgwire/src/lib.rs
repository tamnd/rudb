//! The PostgreSQL wire protocol: frames, messages and the per-connection protocol state.
//!
//! Rank 1 in the layer rule. See `xtask/layers.toml` and `notes/Spec/2140/compat/postgres/16-crate-layout.md`.
//!
//! This crate is the codec of `rudb-server`. It reads bytes into typed messages and writes typed
//! messages into bytes. It has no sockets, no threads, no clock and no TLS, so it is a pure
//! function of its input. A fuzzer can drive it without a socket, and `rudb-postgres` can replay a
//! recorded session into it and compare the output byte for byte. Document 04 of the PostgreSQL
//! compatibility notes is the specification, and the server at the pin is the reference: where
//! the documentation and `postgres.c` disagree, this crate does what `postgres.c` does.
//!
//! # What is here
//!
//! [`split`] and [`split_startup`], which find where one message ends. They check the type byte
//! and the length with the limits of PostgreSQL, and they accept partial input: the server gives
//! them all the bytes it has, and they say how many they used.
//!
//! [`Frontend`], the messages of the client after startup, and [`Packet`], the startup packet and
//! the three requests that come before it. They borrow from the input. A query string or a
//! parameter value is never copied, and it is never converted from the client encoding here.
//!
//! [`OutBuf`], the output of one connection, with one method for each message of the server.
//! [`Backend`] is the same set of messages as values, to read a server back.
//!
//! [`ProtocolError`], which has the SQLSTATE, the text of PostgreSQL for the same bytes, and the
//! [`Level`] that tells the server whether to go on, to send `FATAL`, or to close without a word.
//!
//! # What is not here yet
//!
//! The state machine of a session (startup, negotiation of 3.0 and 3.2, the simple and extended
//! flows, the skip to `Sync` after an error), the SCRAM arithmetic and the cancel key. They come
//! in the next steps of milestone PG1, in this order.
//!
//! # A known difference
//!
//! [`Bind::body`] checks the length of every value before the server converts any of them.
//! PostgreSQL reads and converts the values one by one, so a `Bind` with a bad value in its first
//! parameter and a length past the end in its second gets the conversion error from PostgreSQL
//! and `08P01` from rudb. A client that follows the protocol cannot send such a message.

mod backend;
mod error;
mod frame;
mod frontend;
mod reader;

pub use backend::{Authentication, Backend, Field, Mark, OutBuf, TransactionStatus};
pub use error::{Level, PROTOCOL_VIOLATION, ProtocolError};
pub use frame::{
    AUTH_MESSAGE_LIMIT, CANCEL_KEY_LIMIT, CANCEL_REQUEST_CODE, Cancel, Frame, GSSENC_REQUEST_CODE,
    LARGE_MESSAGE_LIMIT, Mode, PROTOCOL_3_0, PROTOCOL_3_2, Packet, Param, SMALL_MESSAGE_LIMIT,
    SSL_REQUEST_CODE, STARTUP_PACKET_LIMIT, Startup, encode_options, protocol, split,
    split_startup,
};
pub use frontend::{
    Bind, BindBody, Formats, Frontend, FunctionCall, FunctionCallBody, Oids, Target, ValueIter,
    Values, encode_oids,
};
