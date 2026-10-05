//! The databases of the cluster: the rows of `pg_database`.
//!
//! The databases belong to the cluster, as the roles do, so the server keeps them in the file
//! `global/databases` of the data directory, in the format of `global/roles`: a header line with
//! the format number, then one row on each line in the text format of `COPY`.
//!
//! The file of a database is `base/<oid>.rudb`, and its journal is the directory
//! `base/<oid>.rudb.wal`. The name of a database can hold any character but a zero byte, so the
//! name cannot be the name of the file. The OID also stays the same when the database gets a new
//! name, so a rename changes one row and moves no file.

use std::fmt::Write as _;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, PoisonError};

use crate::roles::{BOOTSTRAP_SUPERUSER, FIRST_NORMAL_OID, escape, save, unescape};

/// The file of the databases, relative to the data directory.
pub(crate) const FILE: &str = "global/databases";

/// The header line of the file.
const HEADER: &str = "rudb databases 1";

/// The fixed OIDs of the databases that `initdb` makes, `Template1DbOid`, `Template0DbOid` and
/// `PostgresDbOid`.
pub(crate) const TEMPLATE1: u32 = 1;
pub(crate) const TEMPLATE0: u32 = 4;
pub(crate) const POSTGRES: u32 = 5;

/// The code of `UTF8`, the only encoding that rudb stores.
pub(crate) const UTF8: i32 = 6;

/// The names of the server encodings by code, `pg_enc2name_tbl` up to `PG_ENCODING_BE_LAST`. Code
/// 7 is not used since PostgreSQL 19 removed `MULE_INTERNAL`.
pub(crate) const SERVER_ENCODINGS: [&str; 35] = [
    "SQL_ASCII",
    "EUC_JP",
    "EUC_CN",
    "EUC_KR",
    "EUC_TW",
    "EUC_JIS_2004",
    "UTF8",
    "",
    "LATIN1",
    "LATIN2",
    "LATIN3",
    "LATIN4",
    "LATIN5",
    "LATIN6",
    "LATIN7",
    "LATIN8",
    "LATIN9",
    "LATIN10",
    "WIN1256",
    "WIN1258",
    "WIN866",
    "WIN874",
    "KOI8R",
    "WIN1251",
    "WIN1252",
    "ISO_8859_5",
    "ISO_8859_6",
    "ISO_8859_7",
    "ISO_8859_8",
    "WIN1250",
    "WIN1253",
    "WIN1254",
    "WIN1255",
    "WIN1257",
    "KOI8U",
];

/// The name of a server encoding, `pg_encoding_to_char`, empty for a code that is not one.
pub(crate) fn encoding_name(code: i32) -> &'static str {
    usize::try_from(code).ok().and_then(|at| SERVER_ENCODINGS.get(at)).copied().unwrap_or("")
}

/// The code of a server encoding from its name or an alias, `pg_valid_server_encoding`.
pub(crate) fn encoding_code(name: &str) -> Option<i32> {
    let canonical = rudb_common::guc::encoding(name)?;
    let at = SERVER_ENCODINGS.iter().position(|known| !known.is_empty() && *known == canonical)?;
    i32::try_from(at).ok()
}

/// One row of `pg_database`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Row {
    pub(crate) oid: u32,
    pub(crate) name: String,
    pub(crate) owner: u32,
    pub(crate) encoding: i32,
    /// `datlocprovider`: `b` for builtin, `c` for libc, `i` for ICU.
    pub(crate) provider: char,
    pub(crate) template: bool,
    pub(crate) allow_connections: bool,
    /// The connection limit, -1 for no limit.
    pub(crate) connlimit: i32,
    pub(crate) collate: String,
    pub(crate) ctype: String,
    pub(crate) locale: Option<String>,
    pub(crate) icu_rules: Option<String>,
    pub(crate) collversion: Option<String>,
}

impl Row {
    /// A database with the settings of the databases that `initdb --no-locale` makes.
    pub(crate) fn new(oid: u32, name: &str, owner: u32) -> Row {
        Row {
            oid,
            name: name.to_owned(),
            owner,
            encoding: UTF8,
            provider: 'c',
            template: false,
            allow_connections: true,
            connlimit: -1,
            collate: "C".to_owned(),
            ctype: "C".to_owned(),
            locale: None,
            icu_rules: None,
            collversion: None,
        }
    }
}

/// All the databases of the cluster, in the order of their last change of owner, which is the
/// order in which PostgreSQL lists them in the `DETAIL` of `DROP ROLE`.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct Catalog {
    pub(crate) rows: Vec<Row>,
}

impl Catalog {
    /// The databases after `init`: `template1`, `template0` and `postgres`, owned by the
    /// bootstrap superuser.
    pub(crate) fn bootstrap() -> Catalog {
        let template1 =
            Row { template: true, ..Row::new(TEMPLATE1, "template1", BOOTSTRAP_SUPERUSER) };
        let template0 = Row {
            template: true,
            allow_connections: false,
            ..Row::new(TEMPLATE0, "template0", BOOTSTRAP_SUPERUSER)
        };
        let postgres = Row::new(POSTGRES, "postgres", BOOTSTRAP_SUPERUSER);
        Catalog { rows: vec![template1, template0, postgres] }
    }

    pub(crate) fn find(&self, name: &str) -> Option<&Row> {
        self.rows.iter().find(|row| row.name == name)
    }

    pub(crate) fn by_oid(&self, oid: u32) -> Option<&Row> {
        self.rows.iter().find(|row| row.oid == oid)
    }

    pub(crate) fn by_oid_mut(&mut self, oid: u32) -> Option<&mut Row> {
        self.rows.iter_mut().find(|row| row.oid == oid)
    }

    /// The OID for a new database.
    pub(crate) fn next_oid(&self) -> u32 {
        self.rows.iter().map(|row| row.oid + 1).max().unwrap_or(0).max(FIRST_NORMAL_OID)
    }

    /// Gives the database a new owner and moves its row to the end, as the new `pg_shdepend` row
    /// of PostgreSQL goes after the others.
    pub(crate) fn set_owner(&mut self, oid: u32, owner: u32) {
        if let Some(at) = self.rows.iter().position(|row| row.oid == oid) {
            let mut row = self.rows.remove(at);
            row.owner = owner;
            self.rows.push(row);
        }
    }

    /// The text of the file.
    fn text(&self) -> String {
        let mut out = String::new();
        out.push_str(HEADER);
        out.push('\n');
        let flag = |on: bool| if on { "t" } else { "f" };
        let null =
            |value: &Option<String>| value.as_deref().map_or_else(|| "\\N".to_owned(), escape);
        for row in &self.rows {
            let _ = writeln!(
                out,
                "database\t{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}",
                row.oid,
                escape(&row.name),
                row.owner,
                row.encoding,
                row.provider,
                flag(row.template),
                flag(row.allow_connections),
                row.connlimit,
                escape(&row.collate),
                escape(&row.ctype),
                null(&row.locale),
                null(&row.icu_rules),
                null(&row.collversion),
            );
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
            if fields.len() != 14 || fields[0].as_deref() != Some("database") {
                return Err(bad());
            }
            let text = |i: usize| fields[i].clone().ok_or_else(bad);
            let flag = |i: usize| match text(i)?.as_str() {
                "t" => Ok(true),
                "f" => Ok(false),
                _ => Err(bad()),
            };
            let number = |i: usize| text(i)?.parse::<i64>().map_err(|_| bad());
            let provider = text(5)?;
            catalog.rows.push(Row {
                oid: u32::try_from(number(1)?).map_err(|_| bad())?,
                name: text(2)?,
                owner: u32::try_from(number(3)?).map_err(|_| bad())?,
                encoding: i32::try_from(number(4)?).map_err(|_| bad())?,
                provider: match provider.as_str() {
                    "b" | "c" | "i" => provider.chars().next().unwrap_or('c'),
                    _ => return Err(bad()),
                },
                template: flag(6)?,
                allow_connections: flag(7)?,
                connlimit: i32::try_from(number(8)?).map_err(|_| bad())?,
                collate: text(9)?,
                ctype: text(10)?,
                locale: fields[11].clone(),
                icu_rules: fields[12].clone(),
                collversion: fields[13].clone(),
            });
        }
        Ok(catalog)
    }
}

/// The file of a database.
pub(crate) fn path(data: &Path, oid: u32) -> PathBuf {
    data.join("base").join(format!("{oid}.rudb"))
}

/// The journal directory of a database file, `rudb`'s `<file>.wal`.
pub(crate) fn journal(file: &Path) -> PathBuf {
    let mut name = file.as_os_str().to_owned();
    name.push(".wal");
    PathBuf::from(name)
}

/// Writes the databases to the file of the data directory.
///
/// # Errors
///
/// The text of the error of the system.
pub(crate) fn write(data: &Path, catalog: &Catalog) -> Result<(), String> {
    save(data, FILE, &catalog.text())
}

/// The databases of a running server.
#[derive(Debug)]
pub(crate) struct Databases {
    data: PathBuf,
    catalog: Mutex<Arc<Catalog>>,
}

impl Databases {
    /// Reads the databases of the data directory. A data directory from before this file has one
    /// file for each database, named by the database. These files get the OIDs: the fixed ones
    /// for `template1`, `template0` and `postgres`, and new ones for the others.
    ///
    /// # Errors
    ///
    /// A file that cannot be read, renamed or written, or that is not valid.
    pub(crate) fn open(data: &Path) -> Result<Databases, String> {
        let file = data.join(FILE);
        let catalog = match std::fs::read_to_string(&file) {
            Ok(text) => Catalog::parse(&text)
                .map_err(|e| format!("invalid database file \"{}\": {e}", file.display()))?,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => migrate(data)?,
            Err(e) => return Err(format!("could not read file \"{}\": {e}", file.display())),
        };
        Ok(Databases { data: data.to_owned(), catalog: Mutex::new(Arc::new(catalog)) })
    }

    /// The databases now. A change after this call does not change the copy that it gives.
    pub(crate) fn snapshot(&self) -> Arc<Catalog> {
        self.catalog.lock().unwrap_or_else(PoisonError::into_inner).clone()
    }

    /// Runs `change` on a copy of the databases, writes the copy to the file, and makes it the
    /// databases of the server, as [`crate::roles::Roles::change`] does.
    pub(crate) fn change<T, E>(
        &self,
        change: impl FnOnce(&mut Catalog) -> Result<T, E>,
        write_failed: impl FnOnce(String) -> E,
    ) -> Result<T, E> {
        let mut current = self.catalog.lock().unwrap_or_else(PoisonError::into_inner);
        let mut next = Catalog::clone(&current);
        let done = change(&mut next)?;
        if next != **current {
            write(&self.data, &next).map_err(write_failed)?;
            *current = Arc::new(next);
        }
        Ok(done)
    }
}

/// Makes the file of the databases for a data directory whose database files have the names of
/// the databases, and gives each file the name of its OID.
fn migrate(data: &Path) -> Result<Catalog, String> {
    let base = data.join("base");
    let mut catalog = Catalog::bootstrap();
    let mut names = Vec::new();
    let entries = std::fs::read_dir(&base)
        .map_err(|e| format!("could not open directory \"{}\": {e}", base.display()))?;
    for entry in entries {
        let entry =
            entry.map_err(|e| format!("could not read directory \"{}\": {e}", base.display()))?;
        let file = entry.file_name();
        let Some(name) = file.to_str().and_then(|file| file.strip_suffix(".rudb")) else {
            continue;
        };
        if !name.is_empty() && !name.bytes().all(|b| b.is_ascii_digit()) {
            names.push(name.to_owned());
        }
    }
    names.sort();
    for name in names {
        let oid = match catalog.find(&name) {
            Some(row) => row.oid,
            None => {
                let oid = catalog.next_oid();
                catalog.rows.push(Row::new(oid, &name, BOOTSTRAP_SUPERUSER));
                oid
            }
        };
        let from = base.join(format!("{name}.rudb"));
        let to = path(data, oid);
        for (from, to) in [(journal(&from), journal(&to)), (from, to)] {
            if from.exists() {
                std::fs::rename(&from, &to).map_err(|e| {
                    format!(
                        "could not rename file \"{}\" to \"{}\": {e}",
                        from.display(),
                        to.display()
                    )
                })?;
            }
        }
    }
    write(data, &catalog)?;
    crate::server::log(
        "LOG",
        &format!(
            "created the database file \"{}\" with {} databases",
            data.join(FILE).display(),
            catalog.rows.len()
        ),
    );
    Ok(catalog)
}

#[cfg(test)]
mod tests {
    use super::{Catalog, Row, encoding_code, encoding_name};

    #[test]
    fn the_file_keeps_every_field() {
        let mut catalog = Catalog::bootstrap();
        let mut row = Row::new(catalog.next_oid(), "a/b\tc\\d", 16390);
        row.provider = 'b';
        row.locale = Some("C.UTF-8".to_owned());
        row.connlimit = 7;
        catalog.rows.push(row);
        assert_eq!(catalog.rows[3].oid, 16384);
        assert_eq!(Catalog::parse(&catalog.text()), Ok(catalog));
        assert!(Catalog::parse("rudb databases 1\ndatabase\t1\n").is_err());
        assert!(Catalog::parse("rudb roles 1\n").is_err());
    }

    #[test]
    fn a_new_owner_moves_the_row_to_the_end() {
        let mut catalog = Catalog::bootstrap();
        catalog.set_owner(1, 16384);
        let order: Vec<u32> = catalog.rows.iter().map(|row| row.oid).collect();
        assert_eq!(order, [4, 5, 1]);
        assert_eq!(catalog.by_oid(1).map(|row| row.owner), Some(16384));
    }

    #[test]
    fn the_encodings() {
        assert_eq!(encoding_code("utf-8"), Some(6));
        assert_eq!(encoding_code("UNICODE"), Some(6));
        assert_eq!(encoding_code("latin1"), Some(8));
        assert_eq!(encoding_code("sjis"), None);
        assert_eq!(encoding_code("nosuch"), None);
        assert_eq!(encoding_name(6), "UTF8");
        assert_eq!(encoding_name(7), "");
        assert_eq!(encoding_name(35), "");
        assert_eq!(encoding_name(-1), "");
    }
}
