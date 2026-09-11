//! The values a statement's parameters were given.

use rudb_common::Value;

/// What a prepared statement was handed, by identifier.
///
/// A parameter is written `?`, `?1`, `$1` or `$name`, and the parser gives every one of them an
/// identifier: the number for a positional parameter, the word for a named one, and the position
/// for a bare `?`. So a set of values is a list of identifier and value pairs whatever the statement
/// was written with, and there is one lookup rather than two.
///
/// A list rather than a map. There are a handful of parameters in a statement, never a thousand, and
/// keeping the order they were given in is worth more here than a hash: it is what the error about
/// unused values lists them in.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Parameters {
    values: Vec<(String, Value)>,
}

impl Parameters {
    /// No values, which is what an ordinary statement binds with.
    #[must_use]
    pub const fn new() -> Self {
        Self { values: Vec::new() }
    }

    /// Values by position, numbered from one, which is what `?` and `$1` want.
    #[must_use]
    pub fn positional(values: Vec<Value>) -> Self {
        let values = values
            .into_iter()
            .enumerate()
            .map(|(at, value)| ((at + 1).to_string(), value))
            .collect();
        Self { values }
    }

    /// Gives one parameter a value, replacing whatever it had.
    pub fn set(&mut self, name: impl Into<String>, value: Value) {
        let name = name.into();
        match self.values.iter_mut().find(|(held, _)| same(held, &name)) {
            Some(slot) => slot.1 = value,
            None => self.values.push((name, value)),
        }
    }

    /// What one parameter was given, if it was given anything.
    #[must_use]
    pub fn get(&self, name: &str) -> Option<&Value> {
        self.values.iter().find(|(held, _)| same(held, name)).map(|(_, value)| value)
    }

    /// Whether nothing was provided.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.values.is_empty()
    }

    /// How many were provided.
    #[must_use]
    pub fn len(&self) -> usize {
        self.values.len()
    }

    /// The identifiers, in the order they were given.
    pub fn names(&self) -> impl Iterator<Item = &str> {
        self.values.iter().map(|(name, _)| name.as_str())
    }
}

/// Whether two identifiers are the same parameter.
///
/// Without regard to case, which is measured rather than assumed: duckdb v1.4.1 runs
/// `PREPARE p AS SELECT $A` with `EXECUTE p(a := 1)`. It is the one place the dialect folds case,
/// and it does not fold identifiers anywhere else.
fn same(left: &str, right: &str) -> bool {
    left.eq_ignore_ascii_case(right)
}
