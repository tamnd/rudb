//! The error model.
//!
//! `spec/04-architecture.md` section 4.9 says errors are values, `Result` is everywhere, and no
//! path reachable from user input panics. It also says every error carries a code, a message and
//! optionally a span into the query text, that the codes are stable because clients switch on
//! them, and that the messages match DuckDB's where a DuckDB message is what a test asserts on.
//!
//! This module is where all three of those obligations live.

use std::fmt;

/// The result type used everywhere in the workspace.
pub type Result<T> = std::result::Result<T, Error>;

/// A byte range into the query text.
///
/// Half open, so `start` is the first byte and `end` is one past the last, which is what slicing
/// wants and what every editor protocol in existence expects. Byte offsets rather than character
/// offsets because that is what the parser has and converting is the caller's problem, once, at
/// the point where a human is going to read it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct Span {
    /// First byte of the span.
    pub start: u32,
    /// One past the last byte of the span.
    pub end: u32,
}

impl Span {
    /// A span over `start .. end`.
    #[must_use]
    pub const fn new(start: u32, end: u32) -> Self {
        Self { start, end }
    }

    /// The number of bytes covered, which is zero for a span that points between two characters.
    #[must_use]
    pub const fn len(self) -> u32 {
        self.end.saturating_sub(self.start)
    }

    /// Whether the span covers no bytes.
    #[must_use]
    pub const fn is_empty(self) -> bool {
        self.len() == 0
    }
}

/// What kind of thing went wrong.
///
/// These are stable and they are part of the public interface, because a client that retries on
/// one class of failure and gives up on another has to be able to tell them apart without reading
/// the message. Adding a variant is a compatible change and renaming one is not.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum ErrorCode {
    /// The text is not SQL.
    Parser,
    /// The text is SQL and a value in it is not one the statement can take.
    ///
    /// DuckDB's own line between this and [`ErrorCode::Parser`] is not one anybody would draw
    /// twice, and it is on the wire, so it is here: `SET threads=0` is a syntax error there and a
    /// syntax error here.
    Syntax,
    /// The text is SQL and it does not mean anything, for example a column that is not in scope.
    Binder,
    /// A named object is missing, or one that should be missing is not.
    Catalog,
    /// A value will not convert to the type it is being asked for.
    Conversion,
    /// A value is outside what its type can hold.
    OutOfRange,
    /// An argument is wrong in a way that is not a type error, for example a negative length.
    InvalidInput,
    /// An allocation failed or a memory limit was reached. An error, never an abort.
    OutOfMemory,
    /// The filesystem, the network or the object store said no.
    Io,
    /// It is in the plan and it is not built yet.
    NotImplemented,
    /// A primary key, unique, not null or check constraint was violated.
    Constraint,
    /// A conflict, an abort, or a statement issued outside a transaction that needs one.
    Transaction,
    /// The query was cancelled. Cooperative, checked at morsel boundaries.
    Interrupt,
    /// An invariant this code is responsible for does not hold. Always a bug here, never in the
    /// query.
    Internal,
}

impl ErrorCode {
    /// The prefix DuckDB puts on a message with this code.
    ///
    /// Compatibility obligation from `spec/12-duckdb-compat.md` section 12.5: a great many tests
    /// in the wild assert on the exact text of an error, so the prefix is DuckDB's spelling
    /// including the parts that look like typos. `Not implemented Error` really is capitalised
    /// that way upstream, and `INTERNAL Error` really is shouted.
    #[must_use]
    pub const fn duckdb_name(self) -> &'static str {
        match self {
            Self::Parser => "Parser Error",
            Self::Syntax => "Syntax Error",
            Self::Binder => "Binder Error",
            Self::Catalog => "Catalog Error",
            Self::Conversion => "Conversion Error",
            Self::OutOfRange => "Out of Range Error",
            Self::InvalidInput => "Invalid Input Error",
            Self::OutOfMemory => "Out of Memory Error",
            Self::Io => "IO Error",
            Self::NotImplemented => "Not implemented Error",
            Self::Constraint => "Constraint Error",
            Self::Transaction => "TransactionContext Error",
            Self::Interrupt => "Interrupt Error",
            Self::Internal => "INTERNAL Error",
        }
    }

    /// Whether an error with this code says something about the query rather than about us.
    ///
    /// Used by the fuzzing harness in `spec/16-testing.md` section 16.4, which treats a rejected
    /// query as a normal outcome and an internal error as a finding.
    #[must_use]
    pub const fn is_user_error(self) -> bool {
        matches!(
            self,
            Self::Parser
                | Self::Syntax
                | Self::Binder
                | Self::Catalog
                | Self::Conversion
                | Self::OutOfRange
                | Self::InvalidInput
                | Self::Constraint
                | Self::Transaction
        )
    }
}

impl fmt::Display for ErrorCode {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.duckdb_name())
    }
}

/// An error, carrying a code, a message and optionally where in the query it happened.
///
/// The payload is boxed so that `Error` is one pointer wide, which keeps `Result<T>` the same size
/// as `T` for every `T` that has a niche. Errors are rare and results are returned from every
/// function in the workspace, so the cost belongs on the rare path.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Error(Box<Payload>);

#[derive(Debug, Clone, PartialEq, Eq)]
struct Payload {
    code: ErrorCode,
    message: String,
    span: Option<Span>,
}

impl Error {
    /// An error with a code and a message and no span.
    pub fn new(code: ErrorCode, message: impl Into<String>) -> Self {
        Self(Box::new(Payload { code, message: message.into(), span: None }))
    }

    /// The same error, with the part of the query it is about.
    #[must_use]
    pub fn with_span(mut self, span: Span) -> Self {
        self.0.span = Some(span);
        self
    }

    /// What kind of thing went wrong.
    #[must_use]
    pub fn code(&self) -> ErrorCode {
        self.0.code
    }

    /// The message, without the code prefix that `Display` adds.
    #[must_use]
    pub fn message(&self) -> &str {
        &self.0.message
    }

    /// Where in the query text this is about, if it is about a place.
    #[must_use]
    pub fn span(&self) -> Option<Span> {
        self.0.span
    }

    /// The text is not SQL.
    pub fn parser(message: impl Into<String>) -> Self {
        Self::new(ErrorCode::Parser, message)
    }

    /// The text is SQL and a value in it is not one the statement can take.
    pub fn syntax(message: impl Into<String>) -> Self {
        Self::new(ErrorCode::Syntax, message)
    }

    /// The text is SQL and it does not mean anything.
    pub fn binder(message: impl Into<String>) -> Self {
        Self::new(ErrorCode::Binder, message)
    }

    /// A named object is missing, or one that should be missing is not.
    pub fn catalog(message: impl Into<String>) -> Self {
        Self::new(ErrorCode::Catalog, message)
    }

    /// A value will not convert to the type it is being asked for.
    pub fn conversion(message: impl Into<String>) -> Self {
        Self::new(ErrorCode::Conversion, message)
    }

    /// A value is outside what its type can hold.
    pub fn out_of_range(message: impl Into<String>) -> Self {
        Self::new(ErrorCode::OutOfRange, message)
    }

    /// An argument is wrong in a way that is not a type error.
    pub fn invalid_input(message: impl Into<String>) -> Self {
        Self::new(ErrorCode::InvalidInput, message)
    }

    /// An allocation failed or a memory limit was reached.
    pub fn out_of_memory(message: impl Into<String>) -> Self {
        Self::new(ErrorCode::OutOfMemory, message)
    }

    /// The filesystem, the network or the object store said no.
    pub fn io(message: impl Into<String>) -> Self {
        Self::new(ErrorCode::Io, message)
    }

    /// It is in the plan and it is not built yet.
    pub fn not_implemented(message: impl Into<String>) -> Self {
        Self::new(ErrorCode::NotImplemented, message)
    }

    /// A constraint was violated.
    pub fn constraint(message: impl Into<String>) -> Self {
        Self::new(ErrorCode::Constraint, message)
    }

    /// A conflict, an abort, or a statement issued outside a transaction that needs one.
    pub fn transaction(message: impl Into<String>) -> Self {
        Self::new(ErrorCode::Transaction, message)
    }

    /// The query was cancelled.
    pub fn interrupt(message: impl Into<String>) -> Self {
        Self::new(ErrorCode::Interrupt, message)
    }

    /// An invariant this code is responsible for does not hold.
    ///
    /// Reaching this is always a bug in the database and never a bug in the query, which is why it
    /// reads differently from the others and why the fuzzer treats it as a finding.
    pub fn internal(message: impl Into<String>) -> Self {
        Self::new(ErrorCode::Internal, message)
    }
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}: {}", self.0.code, self.0.message)
    }
}

impl std::error::Error for Error {}

impl From<std::io::Error> for Error {
    fn from(error: std::io::Error) -> Self {
        Self::io(error.to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::{Error, ErrorCode, Span};

    #[test]
    fn an_error_prints_the_way_duckdb_prints_it() {
        let error = Error::binder("Referenced column \"nope\" not found in FROM clause!");
        assert_eq!(
            error.to_string(),
            "Binder Error: Referenced column \"nope\" not found in FROM clause!"
        );
    }

    #[test]
    fn a_result_is_no_wider_than_the_value_in_it() {
        // The reason the payload is boxed. If this ever fails, every function in the workspace
        // got more expensive to return from and nobody noticed.
        assert_eq!(size_of::<Error>(), size_of::<usize>());
        assert_eq!(size_of::<Result<String, Error>>(), size_of::<String>());
    }

    #[test]
    fn a_span_survives_being_attached() {
        let error = Error::parser("syntax error at or near \"FROM\"").with_span(Span::new(7, 11));
        assert_eq!(error.span(), Some(Span::new(7, 11)));
        assert_eq!(error.span().map(Span::len), Some(4));
        assert_eq!(error.code(), ErrorCode::Parser);
    }

    #[test]
    fn the_fuzzer_can_tell_our_bugs_from_the_query_s_bugs() {
        assert!(ErrorCode::Binder.is_user_error());
        assert!(ErrorCode::Conversion.is_user_error());
        assert!(!ErrorCode::Internal.is_user_error());
        assert!(!ErrorCode::OutOfMemory.is_user_error());
        // Not implemented is ours rather than the query's, because the query was legitimate and we
        // are the reason it did not run.
        assert!(!ErrorCode::NotImplemented.is_user_error());
    }
}
