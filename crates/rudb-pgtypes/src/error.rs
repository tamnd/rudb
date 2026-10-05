//! The error of an input function, with the SQLSTATE and the text that PostgreSQL gives.

use rudb_common::SqlState;

/// A value that a type cannot read. The text is the text of PostgreSQL for the same input, so the
/// server can send it in `ErrorResponse` as it is.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TypeError {
    pub sqlstate: SqlState,
    pub message: String,
    pub detail: Option<String>,
    pub hint: Option<String>,
}

impl TypeError {
    pub(crate) fn new(sqlstate: SqlState, message: String) -> TypeError {
        TypeError { sqlstate, message, detail: None, hint: None }
    }

    /// `22P02`, `invalid input syntax for type integer: "1e3"`.
    pub(crate) fn syntax(type_name: &str, input: &str) -> TypeError {
        TypeError::new(
            SqlState::INVALID_TEXT_REPRESENTATION,
            format!("invalid input syntax for type {type_name}: \"{input}\""),
        )
    }

    /// `22003`, `value "99999" is out of range for type smallint`.
    pub(crate) fn range(input: &str, type_name: &str) -> TypeError {
        TypeError::new(
            SqlState::NUMERIC_VALUE_OUT_OF_RANGE,
            format!("value \"{input}\" is out of range for type {type_name}"),
        )
    }
}

impl std::fmt::Display for TypeError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.message)
    }
}

impl std::error::Error for TypeError {}

impl From<TypeError> for rudb_common::Error {
    fn from(error: TypeError) -> rudb_common::Error {
        let mut out = rudb_common::Error::conversion(error.message).state(error.sqlstate);
        if let Some(detail) = error.detail {
            out = out.detail(detail);
        }
        if let Some(hint) = error.hint {
            out = out.hint(hint);
        }
        out
    }
}
