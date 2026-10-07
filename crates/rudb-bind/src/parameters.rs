//! The values a statement's parameters were given.

use std::sync::{Arc, Mutex};

use rudb_common::{DeclaredType, Error, Field, LogicalType, Origin, Value};

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
    written: Vec<(u32, Written)>,
    relations: Vec<(String, Written)>,
    capture: Option<Capture>,
    placeholders: Option<Placeholders>,
    /// The PostgreSQL type that the client declared for a parameter, such as `name` or `oid`,
    /// which a value of its logical type does not tell.
    types: Vec<(String, DeclaredType)>,
}

/// What a statement takes and gives, found by binding it with no values, which is what a client
/// asks before it runs the statement.
///
/// Each parameter is bound as a null. One the caller gave a type is a null of that type. One it
/// did not is a null of no type, which takes its type from the first cast the binder puts on it,
/// the way an untyped parameter in PostgreSQL takes its type from where it is written. Shared, so
/// the caller keeps one end and every binder of the statement fills the other.
#[derive(Debug, Clone, Default)]
pub struct Placeholders(Arc<Mutex<Described>>);

/// What a [`Placeholders`] found.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Described {
    /// Each parameter with the type the caller gave it, or `None`.
    pub declared: Vec<(String, Option<LogicalType>)>,
    /// Each parameter that got a type, with that type, in the order they got it.
    pub resolved: Vec<(String, LogicalType)>,
    /// Each parameter that got a PostgreSQL type that a cast or a column declaration wrote, such
    /// as `varchar(10)`, with that type. The logical type of such a type is in `resolved` too.
    pub written: Vec<(String, DeclaredType)>,
    /// The columns the statement answers: those of a query or of a `RETURNING` list. `None` for a
    /// statement that answers no rows.
    pub fields: Option<Vec<Field>>,
    /// The table column that each of `fields` reads with no change, where there is one.
    pub origins: Vec<Option<Origin>>,
    /// The error of [`crate::fold::planned`] for a query of a PostgreSQL session with no
    /// parameters, which PostgreSQL raises when it plans the query and not when it parses it.
    pub planning: Option<Error>,
}

impl Placeholders {
    /// Parameters with these types, `None` where the type is not known.
    #[must_use]
    pub fn new(declared: Vec<(String, Option<LogicalType>)>) -> Self {
        Self(Arc::new(Mutex::new(Described { declared, ..Described::default() })))
    }

    /// The type the caller gave the parameter `name`, `Some(None)` when it gave none and `None`
    /// when `name` is not one of the parameters.
    #[must_use]
    pub fn declared(&self, name: &str) -> Option<Option<LogicalType>> {
        let described = self.lock();
        described.declared.iter().find(|(held, _)| same(held, name)).map(|(_, ty)| ty.clone())
    }

    /// Gives the parameter `name` the type `ty`, unless it has one already.
    pub fn resolve(&self, name: &str, ty: &LogicalType) {
        let mut described = self.lock();
        if !described.resolved.iter().any(|(held, _)| same(held, name)) {
            described.resolved.push((name.to_string(), ty.clone()));
        }
    }

    /// Gives the parameter `name` the PostgreSQL type `ty`, unless it has one already.
    pub fn resolve_written(&self, name: &str, ty: DeclaredType) {
        let mut described = self.lock();
        if !described.written.iter().any(|(held, _)| same(held, name)) {
            described.written.push((name.to_string(), ty));
        }
    }

    /// Keeps the columns the statement answers.
    pub fn answer(&self, fields: Vec<Field>) {
        self.lock().fields = Some(fields);
    }

    /// Keeps the error that planning the statement would raise.
    pub fn plan_error(&self, error: Option<Error>) {
        self.lock().planning = error;
    }

    /// Whether the statement has no parameters.
    #[must_use]
    pub fn none(&self) -> bool {
        self.lock().declared.is_empty()
    }

    /// Keeps the table column of each column the statement answers.
    pub fn answer_origins(&self, origins: Vec<Option<Origin>>) {
        self.lock().origins = origins;
    }

    /// What was found so far.
    #[must_use]
    pub fn described(&self) -> Described {
        self.lock().clone()
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Described> {
        self.0.lock().unwrap_or_else(std::sync::PoisonError::into_inner)
    }
}

impl PartialEq for Placeholders {
    fn eq(&self, other: &Self) -> bool {
        Arc::ptr_eq(&self.0, &other.0)
    }
}

/// Where a data changing statement leaves the rows it wrote and the rows it removed or changed,
/// which is how a trigger on its table gets them. Shared, so the caller keeps one end and the
/// statement fills the other.
#[derive(Debug, Clone, Default)]
pub struct Capture(Arc<Mutex<Caught>>);

/// What a [`Capture`] caught: the rows as they are now and the rows as they were.
#[derive(Debug, Clone, Default)]
pub struct Caught {
    /// The rows an `INSERT` added or an `UPDATE` wrote.
    pub new: Vec<Vec<Value>>,
    /// The rows a `DELETE` took out or an `UPDATE` changed, as they were before it.
    pub old: Vec<Vec<Value>>,
}

impl Capture {
    /// Adds rows to what was caught.
    pub fn catch(&self, new: Vec<Vec<Value>>, old: Vec<Vec<Value>>) {
        let mut caught = self.0.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        caught.new.extend(new);
        caught.old.extend(old);
    }

    /// The rows caught so far, which leaves it empty.
    #[must_use]
    pub fn take(&self) -> Caught {
        std::mem::take(&mut *self.0.lock().unwrap_or_else(std::sync::PoisonError::into_inner))
    }
}

impl PartialEq for Capture {
    fn eq(&self, other: &Self) -> bool {
        Arc::ptr_eq(&self.0, &other.0)
    }
}

/// The rows a data changing `WITH` definition produced, which it produced before the statement it
/// belongs to was bound.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Written {
    /// The names of its columns.
    pub names: Vec<String>,
    /// The types of its columns.
    pub types: Vec<LogicalType>,
    /// Its rows, each as wide as `names`, so with no `RETURNING` each row is empty.
    pub rows: Vec<Vec<Value>>,
}

impl Parameters {
    /// No values, which is what an ordinary statement binds with.
    #[must_use]
    pub const fn new() -> Self {
        Self {
            values: Vec::new(),
            written: Vec::new(),
            relations: Vec::new(),
            capture: None,
            placeholders: None,
            types: Vec::new(),
        }
    }

    /// No values, with each parameter bound as a placeholder that records what it is used as.
    #[must_use]
    pub fn describing(placeholders: Placeholders) -> Self {
        Self { placeholders: Some(placeholders), ..Self::new() }
    }

    /// The placeholders, when the statement is bound to describe it rather than to run it.
    #[must_use]
    pub fn placeholders(&self) -> Option<&Placeholders> {
        self.placeholders.as_ref()
    }

    /// Values by position, numbered from one, which is what `?` and `$1` want.
    #[must_use]
    pub fn positional(values: Vec<Value>) -> Self {
        let values = values
            .into_iter()
            .enumerate()
            .map(|(at, value)| ((at + 1).to_string(), value))
            .collect();
        Self { values, ..Self::new() }
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

    /// Gives one parameter the PostgreSQL type that the client declared for it.
    pub fn declare(&mut self, name: impl Into<String>, ty: DeclaredType) {
        let name = name.into();
        self.types.retain(|(held, _)| !same(held, &name));
        self.types.push((name, ty));
    }

    /// The PostgreSQL type that the client declared for one parameter, if it declared one.
    #[must_use]
    pub fn declared_type(&self, name: &str) -> Option<DeclaredType> {
        self.types.iter().find(|(held, _)| same(held, name)).map(|(_, ty)| *ty)
    }

    /// Gives the data changing definition at `cte` in the statement the rows it produced.
    pub fn write(&mut self, cte: u32, rows: Written) {
        self.written.retain(|(held, _)| *held != cte);
        self.written.push((cte, rows));
    }

    /// The rows the data changing definition at `cte` produced, if it has run.
    #[must_use]
    pub fn written(&self, cte: u32) -> Option<&Written> {
        self.written.iter().find(|(held, _)| *held == cte).map(|(_, rows)| rows)
    }

    /// Gives the statement a table of rows under a name, which a single part name in it reads ahead
    /// of the catalog. That is how a trigger's body reads the rows that fired it.
    pub fn relate(&mut self, name: impl Into<String>, rows: Written) {
        let name = name.into();
        self.relations.retain(|(held, _)| !held.eq_ignore_ascii_case(&name));
        self.relations.push((name, rows));
    }

    /// The rows handed in under a name, if any were.
    #[must_use]
    pub fn relation(&self, name: &str) -> Option<&Written> {
        self.relations
            .iter()
            .find(|(held, _)| held.eq_ignore_ascii_case(name))
            .map(|(_, rows)| rows)
    }

    /// Asks the statement to leave the rows it writes in `capture`.
    pub fn catch_into(&mut self, capture: Capture) {
        self.capture = Some(capture);
    }

    /// Where the statement leaves the rows it writes, when it was asked to.
    #[must_use]
    pub fn capture(&self) -> Option<&Capture> {
        self.capture.as_ref()
    }

    /// The same values with no capture, which is what a statement run on behalf of this one gets.
    #[must_use]
    pub fn uncaught(&self) -> Self {
        Self { capture: None, ..self.clone() }
    }

    /// Whether nothing was provided.
    ///
    /// Rows a definition wrote count, since a statement bound with them answers for that run alone
    /// and must not be kept or replayed as if it were its text.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.values.is_empty()
            && self.written.is_empty()
            && self.relations.is_empty()
            && self.capture.is_none()
            && self.placeholders.is_none()
            && self.types.is_empty()
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
