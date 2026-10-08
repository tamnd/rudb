//! The notices and the warnings of a statement, which are messages that are not errors.
//!
//! A part of the engine that has something to tell the client raises a [`Notice`] into the sink of
//! the thread that runs the statement. The parse, the bind and the statements that change the
//! catalog run on that thread. The connection empties the sink when a statement starts, and a
//! PostgreSQL server takes the notices of the statement and sends each one as a `NoticeResponse`,
//! before the rows and before the error of the statement (document 11 section 11.9).

use std::cell::RefCell;

/// The severity of a [`Notice`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Level {
    /// `WARNING`.
    Warning,
    /// `NOTICE`.
    Notice,
}

/// A message that a statement gives that is not an error, such as the one for a `DROP TABLE IF
/// EXISTS` of a table that is not there.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Notice {
    /// `WARNING` or `NOTICE`.
    pub level: Level,
    /// The SQLSTATE, which is `00000` for most notices.
    pub sqlstate: &'static str,
    /// The text, in the words of PostgreSQL.
    pub message: String,
    /// The `DETAIL` field.
    pub detail: Option<String>,
    /// The `HINT` field.
    pub hint: Option<String>,
    /// The byte offset in the text of the statement that the notice points to.
    pub position: Option<u32>,
}

impl Notice {
    /// A notice of the severity `level` with no `DETAIL`, no `HINT` and no position.
    #[must_use]
    pub fn new(level: Level, sqlstate: &'static str, message: impl Into<String>) -> Self {
        let message = message.into();
        Self { level, sqlstate, message, detail: None, hint: None, position: None }
    }

    /// The same notice with a `DETAIL`.
    #[must_use]
    pub fn with_detail(mut self, detail: impl Into<String>) -> Self {
        self.detail = Some(detail.into());
        self
    }

    /// The same notice with a `HINT`.
    #[must_use]
    pub fn with_hint(mut self, hint: impl Into<String>) -> Self {
        self.hint = Some(hint.into());
        self
    }

    /// The same notice, pointing to the byte offset `position` of the statement.
    #[must_use]
    pub fn at(mut self, position: u32) -> Self {
        self.position = Some(position);
        self
    }

    /// The notice for a name that a statement did not find and did not need.
    #[must_use]
    pub fn skipped(kind: &str, name: &str) -> Self {
        Self::new(Level::Notice, "00000", format!("{kind} \"{name}\" does not exist, skipping"))
    }

    /// The notice for a name that a statement found and did not make again.
    #[must_use]
    pub fn exists(sqlstate: &'static str, kind: &str, name: &str) -> Self {
        Self::new(Level::Notice, sqlstate, format!("{kind} \"{name}\" already exists, skipping"))
    }
}

thread_local! {
    /// The notices that the statement of this thread raised and that nobody took yet.
    static RAISED: RefCell<Vec<Notice>> = const { RefCell::new(Vec::new()) };
}

/// Puts a notice in the sink of this thread.
pub fn raise(notice: Notice) {
    RAISED.with(|raised| raised.borrow_mut().push(notice));
}

/// Takes the notices of the sink of this thread, in the order they were raised.
#[must_use]
pub fn take() -> Vec<Notice> {
    RAISED.with(|raised| std::mem::take(&mut *raised.borrow_mut()))
}

/// Whether the sink of this thread holds a notice.
#[must_use]
pub fn raised() -> bool {
    RAISED.with(|raised| !raised.borrow().is_empty())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_sink_gives_the_notices_once_and_in_order() {
        drop(take());
        assert!(!raised());
        raise(Notice::skipped("table", "t"));
        raise(Notice::new(Level::Warning, "01000", "w").with_hint("h").at(3));
        assert!(raised());
        let notices = take();
        assert_eq!(notices[0].message, "table \"t\" does not exist, skipping");
        assert_eq!((notices[1].level, notices[1].hint.as_deref()), (Level::Warning, Some("h")));
        assert_eq!(notices[1].position, Some(3));
        assert!(take().is_empty());
    }
}
