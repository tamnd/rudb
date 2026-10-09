//! The values that `ALTER DATABASE SET` and `ALTER ROLE SET` keep for later sessions: the rows of
//! `pg_db_role_setting`.
//!
//! A row belongs to a database and a role, and 0 in either place means each one. The server keeps
//! the rows in the file `global/db_role_settings` of the data directory, in the format of
//! `global/roles`: a header line with the format number, then one row on each line in the text
//! format of `COPY`, with one field for each `name=value` item of `setconfig`.

use std::fmt::Write as _;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, PoisonError};

use rudb_common::guc::Source;

use crate::roles::{escape, save, unescape};

/// The file of the rows, relative to the data directory.
pub(crate) const FILE: &str = "global/db_role_settings";

/// The header line of the file.
const HEADER: &str = "rudb db_role_settings 1";

/// A row of `pg_db_role_setting`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Row {
    /// `setdatabase`, 0 for each database.
    pub(crate) database: u32,
    /// `setrole`, 0 for each role.
    pub(crate) role: u32,
    /// `setconfig`: the items `name=value` in the order in which they were first set.
    pub(crate) config: Vec<String>,
}

/// All the rows.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct Catalog {
    pub(crate) rows: Vec<Row>,
}

impl Catalog {
    /// The items of a database and a role.
    pub(crate) fn config(&self, database: u32, role: u32) -> &[String] {
        self.rows
            .iter()
            .find(|row| row.database == database && row.role == role)
            .map_or(&[], |row| row.config.as_slice())
    }

    /// The items that a session of a role in a database starts with, with their sources, in the
    /// order of `process_settings`: the most specific row first.
    pub(crate) fn session(&self, database: u32, role: u32) -> Vec<(&str, Source)> {
        [
            (database, role, Source::DatabaseUser),
            (0, role, Source::User),
            (database, 0, Source::Database),
            (0, 0, Source::Global),
        ]
        .into_iter()
        .flat_map(|(database, role, source)| {
            self.config(database, role).iter().map(move |item| (item.as_str(), source))
        })
        .collect()
    }

    /// `GUCArrayAdd`: the item for `name` takes the place of the item with the same name, or comes
    /// last.
    pub(crate) fn add(&mut self, database: u32, role: u32, name: &str, value: &str) {
        let item = format!("{name}={value}");
        match self.rows.iter_mut().find(|row| row.database == database && row.role == role) {
            Some(row) => match row.config.iter_mut().find(|old| is_item(old, name)) {
                Some(old) => *old = item,
                None => row.config.push(item),
            },
            None => self.rows.push(Row { database, role, config: vec![item] }),
        }
    }

    /// `GUCArrayDelete`: the item for `name` goes, and the row goes when it has no item left.
    pub(crate) fn delete(&mut self, database: u32, role: u32, name: &str) {
        self.retain(database, role, |item| !is_item(item, name));
    }

    /// Keeps the items of a row for which `keep` is true, and drops the row when none is left.
    pub(crate) fn retain(&mut self, database: u32, role: u32, keep: impl Fn(&str) -> bool) {
        for row in &mut self.rows {
            if row.database == database && row.role == role {
                row.config.retain(|item| keep(item));
            }
        }
        self.rows.retain(|row| !row.config.is_empty());
    }

    /// The text of the file.
    fn text(&self) -> String {
        let mut out = String::new();
        out.push_str(HEADER);
        out.push('\n');
        for row in &self.rows {
            let _ = write!(out, "setting\t{}\t{}", row.database, row.role);
            for item in &row.config {
                out.push('\t');
                out.push_str(&escape(item));
            }
            out.push('\n');
        }
        out
    }

    /// Reads the text of the file.
    fn parse(text: &str) -> Result<Catalog, String> {
        let mut lines = text.lines();
        if lines.next() != Some(HEADER) {
            return Err("the file does not start with the header of the format".to_owned());
        }
        let mut catalog = Catalog::default();
        for (number, line) in lines.enumerate() {
            let bad = || format!("line {} is not valid", number + 2);
            let fields: Vec<Option<String>> = line.split('\t').map(unescape).collect();
            if fields.len() < 4 || fields[0].as_deref() != Some("setting") {
                return Err(bad());
            }
            let oid = |i: usize| fields[i].as_deref().and_then(|f| f.parse::<u32>().ok());
            let config: Option<Vec<String>> = fields[3..].iter().cloned().collect();
            catalog.rows.push(Row {
                database: oid(1).ok_or_else(bad)?,
                role: oid(2).ok_or_else(bad)?,
                config: config.ok_or_else(bad)?,
            });
        }
        Ok(catalog)
    }
}

/// Whether an item `name=value` is for `name`, by the bytes of the name as `GUCArrayAdd` compares
/// them.
fn is_item(item: &str, name: &str) -> bool {
    item.strip_prefix(name).is_some_and(|rest| rest.starts_with('='))
}

/// Splits an item into its name and its value.
pub(crate) fn split(item: &str) -> Option<(&str, &str)> {
    item.split_once('=')
}

/// The rows of a running server.
#[derive(Debug)]
pub(crate) struct DbRoleSettings {
    data: PathBuf,
    catalog: Mutex<Arc<Catalog>>,
}

impl DbRoleSettings {
    /// Reads the rows of the data directory. A data directory without the file has no rows.
    ///
    /// # Errors
    ///
    /// A file that cannot be read or that is not valid.
    pub(crate) fn open(data: &Path) -> Result<DbRoleSettings, String> {
        let file = data.join(FILE);
        let catalog = match std::fs::read_to_string(&file) {
            Ok(text) => Catalog::parse(&text)
                .map_err(|e| format!("invalid settings file \"{}\": {e}", file.display()))?,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Catalog::default(),
            Err(e) => return Err(format!("could not read file \"{}\": {e}", file.display())),
        };
        Ok(DbRoleSettings { data: data.to_owned(), catalog: Mutex::new(Arc::new(catalog)) })
    }

    /// The rows now. A change after this call does not change the copy that it gives.
    pub(crate) fn snapshot(&self) -> Arc<Catalog> {
        self.catalog.lock().unwrap_or_else(PoisonError::into_inner).clone()
    }

    /// Runs `change` on a copy of the rows, writes the copy to the file, and makes it the rows of
    /// the server, as [`crate::roles::Roles::change`] does.
    pub(crate) fn change<T, E>(
        &self,
        change: impl FnOnce(&mut Catalog) -> Result<T, E>,
        write_failed: impl FnOnce(String) -> E,
    ) -> Result<T, E> {
        let mut current = self.catalog.lock().unwrap_or_else(PoisonError::into_inner);
        let mut next = Catalog::clone(&current);
        let done = change(&mut next)?;
        if next != **current {
            save(&self.data, FILE, &next.text()).map_err(write_failed)?;
            *current = Arc::new(next);
        }
        Ok(done)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_items_of_a_row() {
        let mut catalog = Catalog::default();
        catalog.add(16384, 0, "work_mem", "4MB");
        catalog.add(16384, 0, "search_path", "a, \"B\"");
        catalog.add(16384, 0, "work_mem", "8MB");
        catalog.add(0, 10, "DateStyle", "ISO, MDY");
        assert_eq!(catalog.config(16384, 0), ["work_mem=8MB", "search_path=a, \"B\""]);
        catalog.delete(16384, 0, "work");
        catalog.delete(16384, 0, "work_mem");
        assert_eq!(catalog.config(16384, 0), ["search_path=a, \"B\""]);
        catalog.delete(16384, 0, "search_path");
        assert_eq!(catalog.rows.len(), 1, "a row with no item goes");
        assert_eq!(Catalog::parse(&catalog.text()).unwrap(), catalog);
    }

    #[test]
    fn a_session_takes_the_most_specific_row_first() {
        let mut catalog = Catalog::default();
        catalog.add(0, 0, "work_mem", "1MB");
        catalog.add(5, 0, "work_mem", "2MB");
        catalog.add(0, 10, "work_mem", "3MB");
        catalog.add(5, 10, "work_mem\tx", "a\\b\nc");
        catalog.add(6, 10, "work_mem", "5MB");
        assert_eq!(
            catalog.session(5, 10),
            [
                ("work_mem\tx=a\\b\nc", Source::DatabaseUser),
                ("work_mem=3MB", Source::User),
                ("work_mem=2MB", Source::Database),
                ("work_mem=1MB", Source::Global),
            ]
        );
        assert_eq!(Catalog::parse(&catalog.text()).unwrap(), catalog);
    }
}
