//! A trigger, which is a statement the catalog holds against a table and runs when an `INSERT`, an
//! `UPDATE` or a `DELETE` changes that table.

use rudb_parse::quoted;

use crate::QualifiedName;

/// Which statement fires a trigger.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Event {
    /// `INSERT`.
    Insert,
    /// `UPDATE`, or `UPDATE OF` when [`Trigger::columns`] is not empty.
    Update,
    /// `DELETE`.
    Delete,
}

impl Event {
    /// The word `duckdb_triggers()` shows for it.
    #[must_use]
    pub fn word(self) -> &'static str {
        match self {
            Self::Insert => "INSERT",
            Self::Update => "UPDATE",
            Self::Delete => "DELETE",
        }
    }
}

/// One trigger, which lives in the schema of the table it is on.
///
/// The body is held as text and parsed again each time it fires, the way a view is, so it reads
/// the catalog as it is then. [`Trigger::reads`] is what it read when it was made, which is what it
/// depends on: the pin refuses to drop or to rename any of those while the trigger is there.
#[derive(Debug, Clone)]
pub struct Trigger {
    /// Its name, which is unique among the triggers on one table.
    pub name: String,
    /// The table it is on.
    pub table: QualifiedName,
    /// The table it is on as it was written, which is how the pin prints it back.
    pub written_table: String,
    /// Whether it fires before the statement rather than after it.
    pub before: bool,
    /// Which statement fires it.
    pub event: Event,
    /// The columns of `UPDATE OF`, of which a firing `UPDATE` has to set at least one.
    pub columns: Vec<String>,
    /// The name `REFERENCING NEW TABLE AS` gives the rows the statement wrote.
    pub new_table: Option<String>,
    /// The name `REFERENCING OLD TABLE AS` gives the rows the statement removed or changed.
    pub old_table: Option<String>,
    /// Whether it is `FOR EACH ROW` rather than `FOR EACH STATEMENT`.
    pub row: bool,
    /// The statement that runs when it fires.
    pub fired: String,
    /// The body the way the pin prints it.
    pub written: String,
    /// Every table and view the body named, the one it writes included.
    pub reads: Vec<QualifiedName>,
    /// The table the body writes, which is what a chain of triggers follows.
    pub writes: QualifiedName,
    /// What the body does to that table, which is which of the triggers there it fires.
    pub does: Event,
    /// The number the catalog tables join on, given when it is made.
    pub oid: i64,
}

impl Trigger {
    /// The statement that would make it again, which is what `duckdb_triggers()` shows as `sql`.
    #[must_use]
    pub fn sql(&self) -> String {
        let timing = if self.before { "BEFORE" } else { "AFTER" };
        let mut event = self.event.word().to_string();
        if !self.columns.is_empty() {
            let columns = self.columns.iter().map(|column| quoted(column)).collect::<Vec<_>>();
            event += &format!(" OF {}", columns.join(", "));
        }
        let mut referencing = String::new();
        if self.new_table.is_some() || self.old_table.is_some() {
            referencing += " REFERENCING";
            if let Some(name) = &self.new_table {
                referencing += &format!(" NEW TABLE AS {}", quoted(name));
            }
            if let Some(name) = &self.old_table {
                referencing += &format!(" OLD TABLE AS {}", quoted(name));
            }
        }
        let each = if self.row { "ROW" } else { "STATEMENT" };
        format!(
            "CREATE TRIGGER {} {timing} {event} ON {}{referencing} FOR EACH {each} {};",
            quoted(&self.name),
            self.written_table,
            self.written
        )
    }
}
