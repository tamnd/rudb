//! The error model.
//!
//! `spec/04-architecture.md` section 4.9 says errors are values, `Result` is everywhere, and no
//! path reachable from user input panics. It also says every error carries a code, a message and
//! optionally a span into the query text, that the codes are stable because clients switch on
//! them, and that the messages match DuckDB's where a DuckDB message is what a test asserts on.
//!
//! This module is where all three of those obligations live.

use std::fmt;

use crate::sqlstate::{self, SqlState};

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
    /// An entry cannot go because others depend on it, for example a schema that still holds
    /// tables.
    Dependency,
    /// A sequence was asked for a value it cannot give, for example one past its maximum.
    Sequence,
    /// A conflict, an abort, or a statement issued outside a transaction that needs one.
    Transaction,
    /// A setting cannot be applied in the current engine configuration.
    Settings,
    /// The query was cancelled. Cooperative, checked at morsel boundaries.
    Interrupt,
    /// A value is one the function refuses outright rather than one of the wrong type, for example
    /// an empty list handed to `list_reduce` with nothing to start from.
    ParameterNotAllowed,
    /// Two types were asked to meet and cannot, for example two structs of different sizes.
    MismatchType,
    /// A type cannot be used where it was put, for example a list as the key of an index.
    InvalidType,
    /// Something is already held by somebody else, such as a file another database is attached to.
    ResourceInUse,
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
            Self::Dependency => "Dependency Error",
            Self::Sequence => "Sequence Error",
            Self::Transaction => "TransactionContext Error",
            Self::Settings => "Settings Error",
            Self::Interrupt => "INTERRUPT Error",
            Self::ParameterNotAllowed => "Parameter Not Allowed Error",
            Self::MismatchType => "Mismatch Type Error",
            Self::InvalidType => "Invalid type Error",
            Self::ResourceInUse => "Resource In Use Error",
            Self::Internal => "INTERNAL Error",
        }
    }

    /// The SQLSTATE that a PostgreSQL session sends for an error that has no code of its own.
    ///
    /// Each variant covers many codes, so this is right for few errors. It exists so that a
    /// client always gets a code in a sensible class. The place that raises an error knows the
    /// exact code and sets it with [`Error::state`]. `11-errors-and-notices.md` section 11.2 of
    /// the PostgreSQL compatibility notes has the table.
    #[must_use]
    pub const fn fallback_state(self) -> SqlState {
        match self {
            Self::Parser => SqlState::SYNTAX_ERROR,
            Self::Binder => SqlState::SYNTAX_ERROR_OR_ACCESS_RULE_VIOLATION,
            Self::Catalog => SqlState::UNDEFINED_OBJECT,
            Self::Conversion => SqlState::DATA_EXCEPTION,
            Self::Constraint => SqlState::INTEGRITY_CONSTRAINT_VIOLATION,
            Self::Transaction => SqlState::INVALID_TRANSACTION_STATE,
            Self::NotImplemented => SqlState::FEATURE_NOT_SUPPORTED,
            Self::Interrupt => SqlState::QUERY_CANCELED,
            Self::OutOfMemory => SqlState::OUT_OF_MEMORY,
            _ => SqlState::INTERNAL_ERROR,
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
                | Self::Dependency
                | Self::Sequence
                | Self::Transaction
                | Self::Settings
                | Self::ParameterNotAllowed
                | Self::MismatchType
                | Self::InvalidType
                | Self::ResourceInUse
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
    message_only: bool,
    sqlstate: Option<SqlState>,
    fields: Option<Box<Fields>>,
}

/// The optional fields of a PostgreSQL `ErrorResponse`, after the code, the message and the
/// position.
///
/// Most errors have none of them, so the error keeps them behind one more box and pays for them
/// only when one is set. A PostgreSQL session sends each field that is set with its one byte
/// field type. The DuckDB dialect does not send them.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
#[non_exhaustive]
pub struct Fields {
    /// `D`: more facts about the error. Can have several lines.
    pub detail: Option<String>,
    /// `H`: advice on what to do.
    pub hint: Option<String>,
    /// `W`: the context, one line for each level, innermost first.
    pub context: Option<String>,
    /// `s`: the schema of the object.
    pub schema: Option<String>,
    /// `t`: the table.
    pub table: Option<String>,
    /// `c`: the column.
    pub column: Option<String>,
    /// `d`: the data type.
    pub data_type: Option<String>,
    /// `n`: the constraint. Ecto maps an error to a field by this name.
    pub constraint: Option<String>,
    /// `R`: the PostgreSQL routine that raises this error. rudb sets it only where a known client
    /// reads it, for example Rails on a stale cached plan.
    pub routine: Option<String>,
    /// The text that a PostgreSQL session sends in place of the message, when the two dialects
    /// say it differently.
    pub postgres: Option<String>,
    /// Whether PostgreSQL sends no position for this error. An error from running a statement, as
    /// opposed to reading it, has none there.
    pub unplaced: bool,
}

impl Error {
    /// An error with a code and a message and no span.
    pub fn new(code: ErrorCode, message: impl Into<String>) -> Self {
        Self(Box::new(Payload {
            code,
            message: message.into(),
            span: None,
            message_only: false,
            sqlstate: None,
            fields: None,
        }))
    }

    /// The same error, with the part of the query it is about.
    #[must_use]
    pub fn with_span(mut self, span: Span) -> Self {
        self.0.span = Some(span);
        self
    }

    /// Attaches the range only when a more specific caller has not already attached one.
    #[must_use]
    pub fn with_fallback_span(mut self, span: Span) -> Self {
        if self.0.span.is_none() && !span.is_empty() {
            self.0.span = Some(span);
        }
        self
    }

    /// The same error, with the SQLSTATE that a PostgreSQL session sends for it.
    #[must_use]
    pub fn state(mut self, state: SqlState) -> Self {
        self.0.sqlstate = Some(state);
        self
    }

    /// The same error, with the `D` field.
    #[must_use]
    pub fn detail(self, detail: impl Into<String>) -> Self {
        self.field(|fields| fields.detail = Some(detail.into()))
    }

    /// The same error, with the `H` field.
    #[must_use]
    pub fn hint(self, hint: impl Into<String>) -> Self {
        self.field(|fields| fields.hint = Some(hint.into()))
    }

    /// The same error, with the `W` field.
    #[must_use]
    pub fn context(self, context: impl Into<String>) -> Self {
        self.field(|fields| fields.context = Some(context.into()))
    }

    /// The same error, with the `s` and `t` fields.
    #[must_use]
    pub fn table(self, schema: impl Into<String>, table: impl Into<String>) -> Self {
        self.field(|fields| {
            fields.schema = Some(schema.into());
            fields.table = Some(table.into());
        })
    }

    /// The same error, with the `c` field. Set the table too, because a client reads the two
    /// together.
    #[must_use]
    pub fn column(self, column: impl Into<String>) -> Self {
        self.field(|fields| fields.column = Some(column.into()))
    }

    /// The same error, with the `d` field.
    #[must_use]
    pub fn data_type(self, data_type: impl Into<String>) -> Self {
        self.field(|fields| fields.data_type = Some(data_type.into()))
    }

    /// The same error, with the `n` field.
    #[must_use]
    pub fn constraint_name(self, constraint: impl Into<String>) -> Self {
        self.field(|fields| fields.constraint = Some(constraint.into()))
    }

    /// The same error, with the `R` field.
    #[must_use]
    pub fn routine(self, routine: impl Into<String>) -> Self {
        self.field(|fields| fields.routine = Some(routine.into()))
    }

    /// The same error, with the text that a PostgreSQL session sends in place of the message.
    #[must_use]
    pub fn pg(self, message: impl Into<String>) -> Self {
        self.field(|fields| fields.postgres = Some(message.into()))
    }

    /// The same error, which a PostgreSQL session sends with no position.
    #[must_use]
    pub fn unplaced(self) -> Self {
        self.field(|fields| fields.unplaced = true)
    }

    fn field(mut self, set: impl FnOnce(&mut Fields)) -> Self {
        set(self.0.fields.get_or_insert_with(Box::default));
        self
    }

    /// The SQLSTATE that the place that raised this error set, if it set one.
    #[must_use]
    pub fn sqlstate(&self) -> Option<SqlState> {
        self.0.sqlstate
    }

    /// The SQLSTATE to send to a PostgreSQL client.
    ///
    /// This is the code that the raising place set. Without one, it is the fallback of the
    /// [`ErrorCode`], and the fallback is counted in [`sqlstate::fallbacks`]. An internal error is
    /// `XX000` by definition, so it is not counted.
    #[must_use]
    pub fn reported_state(&self) -> SqlState {
        if let Some(state) = self.0.sqlstate {
            return state;
        }
        if self.0.code != ErrorCode::Internal {
            sqlstate::count_fallback();
        }
        self.0.code.fallback_state()
    }

    /// The optional `ErrorResponse` fields, if any is set.
    #[must_use]
    pub fn fields(&self) -> Option<&Fields> {
        self.0.fields.as_deref()
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

    /// Renders this error as the structured JSON form DuckDB returns for `errors_as_json`.
    #[must_use]
    pub fn into_json(mut self) -> Self {
        let exception_type = self.0.code.json_name();
        let subtype = self.0.code.json_subtype(&self.0.message);
        let mut fields = vec![
            ("exception_type", exception_type.to_string()),
            ("exception_message", self.0.message.clone()),
        ];
        if let Some(span) = self.0.span {
            fields.push(("location", format!("[{},{}]", span.start, span.len())));
            fields.push(("position", span.start.to_string()));
        }
        if let Some(subtype) = subtype {
            fields.push(("error_subtype", subtype.to_string()));
        }
        self.0.message = json_object(&fields);
        self.0.message_only = true;
        self
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

    /// A setting cannot be applied in the current engine configuration.
    pub fn settings(message: impl Into<String>) -> Self {
        Self::new(ErrorCode::Settings, message)
    }

    /// A value the function refuses outright.
    pub fn parameter_not_allowed(message: impl Into<String>) -> Self {
        Self::new(ErrorCode::ParameterNotAllowed, message)
    }

    /// An entry that others depend on.
    pub fn dependency(message: impl Into<String>) -> Self {
        Self::new(ErrorCode::Dependency, message)
    }

    /// A sequence that cannot give what was asked of it.
    pub fn sequence(message: impl Into<String>) -> Self {
        Self::new(ErrorCode::Sequence, message)
    }

    /// Two types that cannot meet.
    pub fn mismatch_type(message: impl Into<String>) -> Self {
        Self::new(ErrorCode::MismatchType, message)
    }

    /// A type used where it cannot be.
    pub fn invalid_type(message: impl Into<String>) -> Self {
        Self::new(ErrorCode::InvalidType, message)
    }

    /// The query was cancelled.
    pub fn interrupt(message: impl Into<String>) -> Self {
        Self::new(ErrorCode::Interrupt, message)
    }

    /// An invariant this code is responsible for does not hold.
    ///
    /// Reaching this is always a bug in the database and never a bug in the query, which is why it
    /// reads differently from the others and why the fuzzer treats it as a finding.
    /// Something another holder already has, such as a file attached under another name.
    pub fn resource_in_use(message: impl Into<String>) -> Self {
        Self::new(ErrorCode::ResourceInUse, message)
    }

    pub fn internal(message: impl Into<String>) -> Self {
        Self::new(ErrorCode::Internal, message)
    }
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if self.0.message_only {
            return f.write_str(&self.0.message);
        }
        write!(f, "{}: {}", self.0.code, self.0.message)
    }
}

impl ErrorCode {
    const fn json_name(self) -> &'static str {
        match self {
            Self::Parser => "Parser",
            Self::Syntax => "Syntax",
            Self::Binder => "Binder",
            Self::Catalog => "Catalog",
            Self::Conversion => "Conversion",
            Self::OutOfRange => "Out of Range",
            Self::InvalidInput => "Invalid Input",
            Self::OutOfMemory => "Out of Memory",
            Self::Io => "IO",
            Self::NotImplemented => "Not implemented",
            Self::Constraint => "Constraint",
            Self::Dependency => "Dependency",
            Self::Sequence => "Sequence",
            Self::Transaction => "TransactionContext",
            Self::Settings => "Settings",
            Self::Interrupt => "INTERRUPT",
            Self::ParameterNotAllowed => "Parameter Not Allowed",
            Self::MismatchType => "Mismatch Type",
            Self::InvalidType => "Invalid type",
            Self::ResourceInUse => "Resource In Use",
            Self::Internal => "INTERNAL",
        }
    }

    fn json_subtype(self, message: &str) -> Option<&'static str> {
        match self {
            Self::Parser => Some("SYNTAX_ERROR"),
            Self::Binder if message.starts_with("Referenced column") => Some("COLUMN_NOT_FOUND"),
            Self::Binder if message.contains("No function matches") => Some("NO_MATCHING_FUNCTION"),
            Self::Catalog if message.contains("does not exist") => Some("MISSING_ENTRY"),
            _ => None,
        }
    }
}

fn json_object(fields: &[(&str, String)]) -> String {
    let mut out = String::from("{");
    for (index, (name, value)) in fields.iter().enumerate() {
        if index != 0 {
            out.push(',');
        }
        out.push('"');
        out.push_str(name);
        out.push_str("\":\"");
        for character in value.chars() {
            match character {
                '"' => out.push_str("\\\""),
                '\\' => out.push_str("\\\\"),
                '\n' => out.push_str("\\n"),
                '\r' => out.push_str("\\r"),
                '\t' => out.push_str("\\t"),
                character if character <= '\u{1f}' => {
                    use std::fmt::Write as _;
                    let _ = write!(out, "\\u{:04x}", character as u32);
                }
                character => out.push(character),
            }
        }
        out.push('"');
    }
    out.push('}');
    out
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
    use crate::sqlstate::{self, SqlState};

    #[test]
    fn a_state_set_at_the_raising_place_is_the_one_reported() {
        let error =
            Error::catalog("Table with name t does not exist!").state(SqlState::UNDEFINED_TABLE);
        assert_eq!(error.sqlstate(), Some(SqlState::UNDEFINED_TABLE));
        assert_eq!(error.reported_state().as_str(), "42P01");
    }

    #[test]
    fn an_error_without_a_state_gets_the_fallback_of_its_code() {
        assert_eq!(Error::parser("x").sqlstate(), None);
        let cases = [
            (Error::parser("x"), "42601"),
            (Error::binder("x"), "42000"),
            (Error::catalog("x"), "42704"),
            (Error::conversion("x"), "22000"),
            (Error::constraint("x"), "23000"),
            (Error::transaction("x"), "25000"),
            (Error::not_implemented("x"), "0A000"),
            (Error::interrupt("x"), "57014"),
            (Error::out_of_memory("x"), "53200"),
            (Error::io("x"), "XX000"),
            (Error::internal("x"), "XX000"),
        ];
        for (error, expected) in cases {
            assert_eq!(error.reported_state().as_str(), expected, "{error}");
        }
    }

    #[test]
    fn a_fallback_is_counted() {
        let before = sqlstate::fallbacks();
        let _ = Error::binder("x").reported_state();
        assert!(sqlstate::fallbacks() > before);
    }

    #[test]
    fn the_response_fields_are_kept_and_do_not_change_the_text() {
        let error = Error::constraint("Duplicate key \"id: 1\" violates primary key constraint.")
            .state(SqlState::UNIQUE_VIOLATION)
            .detail("Key (id)=(1) already exists.")
            .table("public", "t")
            .constraint_name("t_pkey");
        let fields = error.fields().expect("fields were set");
        assert_eq!(fields.detail.as_deref(), Some("Key (id)=(1) already exists."));
        assert_eq!(fields.schema.as_deref(), Some("public"));
        assert_eq!(fields.table.as_deref(), Some("t"));
        assert_eq!(fields.constraint.as_deref(), Some("t_pkey"));
        assert_eq!(fields.hint, None);
        assert_eq!(
            error.to_string(),
            "Constraint Error: Duplicate key \"id: 1\" violates primary key constraint."
        );
        assert_eq!(Error::binder("x").fields(), None);
    }

    #[test]
    fn an_error_prints_the_way_duckdb_prints_it() {
        let error = Error::binder("Referenced column \"nope\" not found in FROM clause!");
        assert_eq!(
            error.to_string(),
            "Binder Error: Referenced column \"nope\" not found in FROM clause!"
        );
    }

    #[test]
    fn a_json_error_is_structured_and_has_no_text_prefix() {
        let error = Error::binder("Referenced column \"nope\" not found\nnext")
            .with_span(Span::new(7, 11))
            .into_json();
        assert_eq!(
            error.to_string(),
            "{\"exception_type\":\"Binder\",\"exception_message\":\"Referenced column \\\"nope\\\" not found\\nnext\",\"location\":\"[7,4]\",\"position\":\"7\",\"error_subtype\":\"COLUMN_NOT_FOUND\"}"
        );
        assert_eq!(error.code(), ErrorCode::Binder);
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
    fn a_fallback_span_keeps_the_more_specific_range() {
        let specific = Error::binder("missing")
            .with_span(Span::new(7, 14))
            .with_fallback_span(Span::new(0, 20));
        assert_eq!(specific.span(), Some(Span::new(7, 14)));
        let fallback = Error::binder("missing").with_fallback_span(Span::new(0, 20));
        assert_eq!(fallback.span(), Some(Span::new(0, 20)));
        assert_eq!(Error::binder("missing").with_fallback_span(Span::new(0, 0)).span(), None);
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
