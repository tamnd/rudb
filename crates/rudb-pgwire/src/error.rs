//! The error that the codec gives for bytes that break the protocol.

use std::fmt;

/// SQLSTATE `08P01`, `protocol_violation`. Most errors of this crate have it. The checks of the
/// startup packet use three more codes, as PostgreSQL does.
pub const PROTOCOL_VIOLATION: &str = "08P01";

/// What the server does after a [`ProtocolError`].
///
/// PostgreSQL reports a protocol error at one of three levels, and the level is part of what the
/// client sees. So the codec gives the level with each error, and the server does not choose it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Level {
    /// The server sends `ErrorResponse` with severity `ERROR` and the session continues. In the
    /// extended protocol the server then skips messages until the next `Sync`.
    Error,
    /// The server sends `ErrorResponse` with severity `FATAL` and closes the connection.
    Fatal,
    /// The server writes the message to its log and closes the connection without a word to the
    /// client. PostgreSQL uses the level `COMMERROR` for these. The frame boundary is lost, so the
    /// client cannot read an answer at a known place.
    Log,
}

/// Bytes from the client that break the protocol.
///
/// The message is the text that PostgreSQL gives for the same bytes, so that a client sees the
/// same failure from rudb.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProtocolError {
    pub level: Level,
    pub sqlstate: &'static str,
    pub message: String,
    /// The `D` field of the error, if PostgreSQL sends one for the same bytes.
    pub detail: Option<&'static str>,
    /// The `H` field of the error, if PostgreSQL sends one for the same bytes.
    pub hint: Option<&'static str>,
}

impl ProtocolError {
    pub(crate) fn new(
        level: Level,
        sqlstate: &'static str,
        message: impl Into<String>,
    ) -> ProtocolError {
        ProtocolError { level, sqlstate, message: message.into(), detail: None, hint: None }
    }

    pub(crate) fn error(message: impl Into<String>) -> ProtocolError {
        ProtocolError::new(Level::Error, PROTOCOL_VIOLATION, message)
    }

    pub(crate) fn fatal(message: impl Into<String>) -> ProtocolError {
        ProtocolError::new(Level::Fatal, PROTOCOL_VIOLATION, message)
    }

    pub(crate) fn log(message: impl Into<String>) -> ProtocolError {
        ProtocolError::new(Level::Log, PROTOCOL_VIOLATION, message)
    }

    pub(crate) fn with_detail(mut self, detail: &'static str) -> ProtocolError {
        self.detail = Some(detail);
        self
    }

    pub(crate) fn with_hint(mut self, hint: &'static str) -> ProtocolError {
        self.hint = Some(hint);
        self
    }

    /// The error for the next read after [`split`](crate::split) gave a [`Level::Error`] for a
    /// type byte in [`Mode::CopyIn`](crate::Mode::CopyIn).
    ///
    /// PostgreSQL reads the type byte, rejects it and recovers from the error without the rest
    /// of the frame. It then sends `ReadyForQuery`, and the next read finds that the last message
    /// was never finished. The server must give this error at that point.
    pub fn lost_sync() -> ProtocolError {
        ProtocolError::fatal("terminating connection because protocol synchronization was lost")
    }
}

impl fmt::Display for ProtocolError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{} ({})", self.message, self.sqlstate)
    }
}

impl std::error::Error for ProtocolError {}
