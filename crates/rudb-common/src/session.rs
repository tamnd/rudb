//! What a session has set, for the tables and functions that read a setting back.
//!
//! Here for the layer rule and not because a setting is a kind of value, which is the same reason
//! [`crate::Cancel`] is here. The thing that fills this in is the embedding API at rank 13, which is
//! the only place that knows what `SET memory_limit` left behind, and the thing that reads it is the
//! executor at rank 12, where `duckdb_settings()` is built. No two crates in between can see each
//! other, so the only place both of them can see is the bottom.
//!
//! Strings on both sides, rather than a value per setting. A setting is written as text by `SET`,
//! read back as text by `current_setting()` and printed as text by `duckdb_settings()`, and the one
//! place the type matters is the `input_type` column, which is a fact about the setting rather than
//! about the session. Holding a `Value` here would mean the rendering happened twice, once for each
//! reader, and the two would eventually disagree about how many decimal places a memory limit has.

use std::collections::BTreeMap;

/// The settings a session has, by name.
///
/// Every setting the engine has, not only the ones somebody changed. A reader of this is answering
/// "what is it now", so a name that is missing means the engine does not have that setting rather
/// than that it is at its default.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Session {
    values: BTreeMap<String, String>,
}

impl Session {
    /// A session that knows nothing, which is what a caller with no database behind it has.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Records what one setting is now.
    pub fn set(&mut self, name: &str, value: impl Into<String>) {
        self.values.insert(name.to_string(), value.into());
    }

    /// What that setting is now, and `None` for a name this session has no answer for.
    #[must_use]
    pub fn get(&self, name: &str) -> Option<&str> {
        self.values.get(name).map(String::as_str)
    }

    /// Every setting and its value, in name order.
    pub fn iter(&self) -> impl Iterator<Item = (&str, &str)> {
        self.values.iter().map(|(name, value)| (name.as_str(), value.as_str()))
    }

    /// Whether nothing has been recorded.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.values.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::Session;

    #[test]
    fn a_session_hands_back_what_was_put_in_and_says_nothing_about_a_name_it_has_not_got() {
        let mut session = Session::new();
        assert!(session.is_empty());
        session.set("threads", "8");
        session.set("memory_limit", "1.0 GiB");
        assert_eq!(session.get("threads"), Some("8"));
        assert_eq!(session.get("nothing_called_this"), None);
        // Name order, because the one reader of this is a catalog table that comes out sorted and
        // sorting it twice would be sorting it once too many.
        let pairs: Vec<(&str, &str)> = session.iter().collect();
        assert_eq!(pairs, [("memory_limit", "1.0 GiB"), ("threads", "8")]);
    }
}
