//! The roles of the cluster: the rows of `pg_authid`, and the rows of `pg_auth_members` that the
//! server reads for the ADMIN option.
//!
//! The roles belong to the cluster and not to one database, as in PostgreSQL, so the server keeps
//! them itself in the file `global/roles` of the data directory. The first line is a header with
//! the format number. Each other line is one row in the text format of `COPY`: a tab between the
//! fields, `\N` for a null, and a backslash before a tab, a newline, a carriage return or a
//! backslash in a value. The first field tells the table, `role` or `member`.
//!
//! A change writes a new file and renames it over the old one, so a crash leaves the old roles or
//! the new roles and never a part of a change.

use std::fmt::Write as _;
use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, PoisonError};

use rudb_pgwire::{SCRAM_SALT_LEN, ScramSecret};

use crate::crypto::Provider;

/// The OID of the bootstrap superuser, `BOOTSTRAP_SUPERUSERID`.
pub(crate) const BOOTSTRAP_SUPERUSER: u32 = 10;

/// The first OID of a role that is not made by `init`, `FirstNormalObjectId`.
const FIRST_NORMAL_OID: u32 = 16384;

/// The file of the roles, relative to the data directory.
pub(crate) const FILE: &str = "global/roles";

/// The header line of the file.
const HEADER: &str = "rudb roles 1";

/// One row of `pg_authid`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Role {
    pub(crate) oid: u32,
    pub(crate) name: String,
    pub(crate) superuser: bool,
    pub(crate) inherit: bool,
    pub(crate) createrole: bool,
    pub(crate) createdb: bool,
    pub(crate) login: bool,
    pub(crate) replication: bool,
    pub(crate) bypassrls: bool,
    /// The connection limit, -1 for no limit.
    pub(crate) connlimit: i32,
    /// The stored secret: SCRAM, MD5, or `None` for no password.
    pub(crate) password: Option<String>,
    /// The end of the password, in microseconds since 2000-01-01 UTC.
    pub(crate) valid_until: Option<i64>,
}

impl Role {
    /// A role with the defaults of `CREATE ROLE`.
    pub(crate) fn new(oid: u32, name: &str) -> Role {
        Role {
            oid,
            name: name.to_owned(),
            superuser: false,
            inherit: true,
            createrole: false,
            createdb: false,
            login: false,
            replication: false,
            bypassrls: false,
            connlimit: -1,
            password: None,
            valid_until: None,
        }
    }
}

/// One row of `pg_auth_members`: `member` is a member of `role`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Member {
    pub(crate) role: u32,
    pub(crate) member: u32,
    pub(crate) grantor: u32,
    pub(crate) admin: bool,
    pub(crate) inherit: bool,
    pub(crate) set: bool,
}

/// All the roles of the cluster.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct Catalog {
    pub(crate) roles: Vec<Role>,
    pub(crate) members: Vec<Member>,
}

impl Catalog {
    /// The roles after `init`: the bootstrap superuser alone.
    pub(crate) fn bootstrap(name: &str, password: Option<String>) -> Catalog {
        let role = Role {
            superuser: true,
            createrole: true,
            createdb: true,
            login: true,
            replication: true,
            bypassrls: true,
            password,
            ..Role::new(BOOTSTRAP_SUPERUSER, name)
        };
        Catalog { roles: vec![role], members: Vec::new() }
    }

    pub(crate) fn find(&self, name: &str) -> Option<&Role> {
        self.roles.iter().find(|role| role.name == name)
    }

    pub(crate) fn by_oid(&self, oid: u32) -> Option<&Role> {
        self.roles.iter().find(|role| role.oid == oid)
    }

    pub(crate) fn by_oid_mut(&mut self, oid: u32) -> Option<&mut Role> {
        self.roles.iter_mut().find(|role| role.oid == oid)
    }

    /// The OID for a new role.
    pub(crate) fn next_oid(&self) -> u32 {
        self.roles.iter().map(|role| role.oid + 1).max().unwrap_or(0).max(FIRST_NORMAL_OID)
    }

    /// `superuser_arg`.
    pub(crate) fn superuser(&self, oid: u32) -> bool {
        self.by_oid(oid).is_some_and(|role| role.superuser)
    }

    /// `has_createrole_privilege`: a superuser has it too.
    pub(crate) fn createrole(&self, oid: u32) -> bool {
        self.by_oid(oid).is_some_and(|role| role.superuser || role.createrole)
    }

    /// `is_admin_of_role`: a superuser is the admin of every role, and no role is the admin of
    /// itself. Otherwise `member`, or a role that `member` is a member of, needs the ADMIN option
    /// on `role`.
    pub(crate) fn is_admin(&self, member: u32, role: u32) -> bool {
        if self.superuser(member) {
            return true;
        }
        if member == role {
            return false;
        }
        let mut seen = vec![member];
        let mut at = 0;
        while let Some(&now) = seen.get(at) {
            at += 1;
            for grant in self.members.iter().filter(|grant| grant.member == now) {
                if grant.role == role && grant.admin {
                    return true;
                }
                if !seen.contains(&grant.role) {
                    seen.push(grant.role);
                }
            }
        }
        false
    }

    /// `member_can_set_role`: a superuser can become any role, and another role can become
    /// itself and the roles that it is a member of with the SET option at each step.
    pub(crate) fn can_set(&self, member: u32, role: u32) -> bool {
        if self.superuser(member) || member == role {
            return true;
        }
        let mut seen = vec![member];
        let mut at = 0;
        while let Some(&now) = seen.get(at) {
            at += 1;
            for grant in self.members.iter().filter(|grant| grant.member == now && grant.set) {
                if grant.role == role {
                    return true;
                }
                if !seen.contains(&grant.role) {
                    seen.push(grant.role);
                }
            }
        }
        false
    }

    /// The text of the file.
    fn text(&self) -> String {
        let mut out = String::new();
        out.push_str(HEADER);
        out.push('\n');
        let flag = |on: bool| if on { "t" } else { "f" };
        for role in &self.roles {
            let _ = writeln!(
                out,
                "role\t{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}",
                role.oid,
                escape(&role.name),
                flag(role.superuser),
                flag(role.inherit),
                flag(role.createrole),
                flag(role.createdb),
                flag(role.login),
                flag(role.replication),
                flag(role.bypassrls),
                role.connlimit,
                role.password.as_deref().map_or_else(|| "\\N".to_owned(), escape),
                role.valid_until.map_or_else(|| "\\N".to_owned(), |at| at.to_string()),
            );
        }
        for grant in &self.members {
            let _ = writeln!(
                out,
                "member\t{}\t{}\t{}\t{}\t{}\t{}",
                grant.role,
                grant.member,
                grant.grantor,
                flag(grant.admin),
                flag(grant.inherit),
                flag(grant.set),
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
            let text = |i: usize| fields.get(i).cloned().flatten().ok_or_else(bad);
            let flag = |i: usize| match text(i)?.as_str() {
                "t" => Ok(true),
                "f" => Ok(false),
                _ => Err(bad()),
            };
            let oid = |i: usize| text(i)?.parse::<u32>().map_err(|_| bad());
            match (fields.first().cloned().flatten().as_deref(), fields.len()) {
                (Some("role"), 13) => catalog.roles.push(Role {
                    oid: oid(1)?,
                    name: text(2)?,
                    superuser: flag(3)?,
                    inherit: flag(4)?,
                    createrole: flag(5)?,
                    createdb: flag(6)?,
                    login: flag(7)?,
                    replication: flag(8)?,
                    bypassrls: flag(9)?,
                    connlimit: text(10)?.parse().map_err(|_| bad())?,
                    password: fields[11].clone(),
                    valid_until: match &fields[12] {
                        Some(at) => Some(at.parse().map_err(|_| bad())?),
                        None => None,
                    },
                }),
                (Some("member"), 7) => catalog.members.push(Member {
                    role: oid(1)?,
                    member: oid(2)?,
                    grantor: oid(3)?,
                    admin: flag(4)?,
                    inherit: flag(5)?,
                    set: flag(6)?,
                }),
                _ => return Err(bad()),
            }
        }
        Ok(catalog)
    }
}

/// A value in the text format of `COPY`.
fn escape(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    for c in value.chars() {
        match c {
            '\\' => out.push_str("\\\\"),
            '\t' => out.push_str("\\t"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            c => out.push(c),
        }
    }
    out
}

/// The value of a field in the text format of `COPY`, `None` for `\N`.
fn unescape(field: &str) -> Option<String> {
    if field == "\\N" {
        return None;
    }
    let mut out = String::with_capacity(field.len());
    let mut chars = field.chars();
    while let Some(c) = chars.next() {
        if c != '\\' {
            out.push(c);
            continue;
        }
        match chars.next() {
            Some('t') => out.push('\t'),
            Some('n') => out.push('\n'),
            Some('r') => out.push('\r'),
            Some(other) => out.push(other),
            None => out.push('\\'),
        }
    }
    Some(out)
}

/// Writes the roles to the file of the data directory, through a temporary file and a rename.
///
/// # Errors
///
/// The text of the error of the system, as PostgreSQL gives it for a file that it cannot write.
pub(crate) fn write(data: &Path, catalog: &Catalog) -> Result<(), String> {
    let path = data.join(FILE);
    let dir = path.parent().unwrap_or(data);
    std::fs::create_dir_all(dir)
        .map_err(|e| format!("could not create directory \"{}\": {e}", dir.display()))?;
    let temp = path.with_extension("tmp");
    let failed = |e: std::io::Error| format!("could not write file \"{}\": {e}", temp.display());
    let mut file = std::fs::File::create(&temp).map_err(failed)?;
    file.write_all(catalog.text().as_bytes()).map_err(failed)?;
    file.sync_all().map_err(failed)?;
    drop(file);
    std::fs::rename(&temp, &path).map_err(|e| {
        format!("could not rename file \"{}\" to \"{}\": {e}", temp.display(), path.display())
    })?;
    // The rename is durable when the directory is.
    if let Ok(dir) = std::fs::File::open(dir) {
        let _ = dir.sync_all();
    }
    Ok(())
}

/// The name of the user that runs the process, which `initdb` takes for the bootstrap superuser
/// when no name is given.
pub fn os_user() -> String {
    // SAFETY: `geteuid` cannot fail, and `getpwuid` gives null or a record that stays valid until
    // the next call, which comes after the name is copied.
    let name = unsafe {
        let record = libc::getpwuid(libc::geteuid());
        if record.is_null() || (*record).pw_name.is_null() {
            None
        } else {
            Some(std::ffi::CStr::from_ptr((*record).pw_name).to_string_lossy().into_owned())
        }
    };
    name.or_else(|| std::env::var("USER").ok()).unwrap_or_else(|| "postgres".to_owned())
}

/// A password in clear text as a SCRAM secret with a new random salt, as `pg_be_scram_build_secret`
/// makes it.
pub(crate) fn scram(password: &str, iterations: i32) -> String {
    let salt = crate::poll::random::<SCRAM_SALT_LEN>();
    ScramSecret::build(&Provider, password.as_bytes(), &salt, iterations).to_string()
}

/// The roles of a running server.
#[derive(Debug)]
pub(crate) struct Roles {
    data: PathBuf,
    catalog: Mutex<Arc<Catalog>>,
}

impl Roles {
    /// Reads the roles of the data directory. A data directory from before the roles has no file,
    /// and gets one with the user of the process as the bootstrap superuser, as `initdb` would
    /// make it.
    ///
    /// # Errors
    ///
    /// A file that cannot be read or that is not valid.
    pub(crate) fn open(data: &Path) -> Result<Roles, String> {
        let path = data.join(FILE);
        let catalog = match std::fs::read_to_string(&path) {
            Ok(text) => Catalog::parse(&text)
                .map_err(|e| format!("invalid role file \"{}\": {e}", path.display()))?,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                let name = os_user();
                let catalog = Catalog::bootstrap(&name, None);
                write(data, &catalog)?;
                crate::server::log(
                    "LOG",
                    &format!(
                        "created the role file \"{}\" with the superuser \"{name}\"",
                        path.display()
                    ),
                );
                catalog
            }
            Err(e) => return Err(format!("could not read file \"{}\": {e}", path.display())),
        };
        Ok(Roles { data: data.to_owned(), catalog: Mutex::new(Arc::new(catalog)) })
    }

    /// The roles now. A change after this call does not change the copy that it gives.
    pub(crate) fn snapshot(&self) -> Arc<Catalog> {
        self.catalog.lock().unwrap_or_else(PoisonError::into_inner).clone()
    }

    /// Runs `change` on a copy of the roles, writes the copy to the file, and makes it the roles
    /// of the server. An error of `change` or of the write leaves the roles as they were. The
    /// lock is held for the whole change, so two changes cannot lose each other.
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

#[cfg(test)]
mod tests {
    use super::{BOOTSTRAP_SUPERUSER, Catalog, Member, Role};

    #[test]
    fn the_file_keeps_every_field() {
        let mut catalog = Catalog::bootstrap("post\tgres", Some("SCRAM-SHA-256$4096:x".to_owned()));
        let mut role = Role::new(catalog.next_oid(), "a \\ b\nc");
        role.connlimit = 3;
        role.valid_until = Some(-12);
        catalog.roles.push(role);
        catalog.members.push(Member {
            role: 16384,
            member: BOOTSTRAP_SUPERUSER,
            grantor: BOOTSTRAP_SUPERUSER,
            admin: true,
            inherit: false,
            set: false,
        });
        assert_eq!(catalog.roles[1].oid, 16384);
        assert_eq!(Catalog::parse(&catalog.text()), Ok(catalog));
        assert!(Catalog::parse("rudb roles 1\nrole\t1\n").is_err());
        assert!(Catalog::parse("other\n").is_err());
    }

    #[test]
    fn the_admin_option() {
        let mut catalog = Catalog::bootstrap("postgres", None);
        for (oid, name) in [(16384, "a"), (16385, "b"), (16386, "c")] {
            catalog.roles.push(Role::new(oid, name));
        }
        let grant = |role, member, admin| Member {
            role,
            member,
            grantor: BOOTSTRAP_SUPERUSER,
            admin,
            inherit: false,
            set: false,
        };
        catalog.members.push(grant(16385, 16384, true));
        catalog.members.push(grant(16384, 16386, false));
        assert!(catalog.is_admin(BOOTSTRAP_SUPERUSER, 16384));
        assert!(catalog.is_admin(16384, 16385));
        assert!(!catalog.is_admin(16385, 16385));
        // `c` is a member of `a`, which has the ADMIN option on `b`.
        assert!(catalog.is_admin(16386, 16385));
        assert!(!catalog.is_admin(16385, 16384));
    }
}
