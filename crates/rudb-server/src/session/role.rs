//! `CREATE ROLE`, `ALTER ROLE` and `DROP ROLE`, with their forms for `USER` and `GROUP`, which
//! the server runs itself on the roles of the cluster.
//!
//! The reader follows the rules of `gram.y` for these statements, and the checks run in the order
//! of `CreateRole`, `AlterRole`, `RenameRole` and `DropRole` in `user.c`, so that a statement
//! fails with the same error as in PostgreSQL. A change of the roles is not part of the
//! transaction: it stays when the transaction rolls back.

use rudb_common::Fields;
use rudb_common::guc::Settings;
use rudb_pgtypes::{DateTimeInput, timestamptz_in};
use rudb_pgwire::{CommandTag, OutBuf, PasswordType, md5_encrypt, verify_password};

use super::Failure;
use super::setting::{Token, spanned};
use crate::crypto::Provider;
use crate::roles::{BOOTSTRAP_SUPERUSER, Catalog, Member, Role, Roles, scram};

/// The longest name, `NAMEDATALEN - 1`. The scanner cuts a longer name.
const NAME_LIMIT: usize = 63;

/// The longest stored secret, `MAX_ENCRYPTED_PASSWORD_LEN`.
const SECRET_LIMIT: usize = 512;

/// The reserved key words of PostgreSQL, which cannot be a role name without quotes.
const RESERVED: &[&str] = &[
    "all",
    "analyse",
    "analyze",
    "and",
    "any",
    "array",
    "as",
    "asc",
    "asymmetric",
    "both",
    "case",
    "cast",
    "check",
    "collate",
    "column",
    "constraint",
    "create",
    "current_catalog",
    "current_date",
    "current_role",
    "current_time",
    "current_timestamp",
    "current_user",
    "default",
    "deferrable",
    "desc",
    "distinct",
    "do",
    "else",
    "end",
    "except",
    "false",
    "fetch",
    "for",
    "foreign",
    "from",
    "grant",
    "group",
    "having",
    "in",
    "initially",
    "intersect",
    "into",
    "lateral",
    "leading",
    "limit",
    "localtime",
    "localtimestamp",
    "not",
    "null",
    "offset",
    "on",
    "only",
    "or",
    "order",
    "placing",
    "primary",
    "references",
    "returning",
    "select",
    "session_user",
    "some",
    "symmetric",
    "system_user",
    "table",
    "then",
    "to",
    "trailing",
    "true",
    "union",
    "unique",
    "user",
    "using",
    "variadic",
    "when",
    "where",
    "window",
    "with",
];

/// The key words of the options of a role. They are not names, so they cannot be an option of
/// the form `IDENT` such as `LOGIN`.
const OPTION_WORDS: &[&str] = &[
    "password",
    "encrypted",
    "unencrypted",
    "inherit",
    "connection",
    "valid",
    "sysid",
    "admin",
    "role",
    "in",
    "user",
    "rename",
    "set",
    "reset",
];

/// A role in a statement, `RoleSpec`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(in crate::session) enum Spec {
    Name(String),
    Public,
    CurrentRole,
    CurrentUser,
    SessionUser,
}

/// An attribute of a role that an option sets.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(in crate::session) enum Attribute {
    Superuser,
    Inherit,
    CreateRole,
    CreateDb,
    Login,
    Replication,
    BypassRls,
}

/// The value of an option of a role.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(in crate::session) enum Value {
    /// `PASSWORD 'text'`, or `PASSWORD NULL` as `None`.
    Password(Option<String>),
    Attribute(Attribute, bool),
    ConnectionLimit(i32),
    ValidUntil(String),
    Sysid,
    /// `IN ROLE`, `IN GROUP`, `ROLE`, `ADMIN` and `USER`, which change the members of roles.
    Members(&'static str),
}

/// An option and the place where it starts in the statement.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(in crate::session) struct Opt {
    value: Value,
    at: usize,
}

impl Opt {
    /// The name of the option in `user.c`, for the check that it is not given twice.
    fn key(&self) -> &'static str {
        match self.value {
            Value::Password(_) => "password",
            Value::Attribute(Attribute::Superuser, _) => "superuser",
            Value::Attribute(Attribute::Inherit, _) => "inherit",
            Value::Attribute(Attribute::CreateRole, _) => "createrole",
            Value::Attribute(Attribute::CreateDb, _) => "createdb",
            Value::Attribute(Attribute::Login, _) => "canlogin",
            Value::Attribute(Attribute::Replication, _) => "isreplication",
            Value::Attribute(Attribute::BypassRls, _) => "bypassrls",
            Value::ConnectionLimit(_) => "connectionlimit",
            Value::ValidUntil(_) => "validUntil",
            Value::Sysid => "sysid",
            Value::Members(key) => key,
        }
    }
}

/// A statement on the roles.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(in crate::session) enum Statement {
    /// `CREATE ROLE`, and `CREATE USER` with `user` true, which logs in by default.
    Create {
        name: String,
        user: bool,
        options: Vec<Opt>,
    },
    Alter {
        role: Spec,
        options: Vec<Opt>,
    },
    Rename {
        from: String,
        to: String,
    },
    Drop {
        roles: Vec<Spec>,
        missing_ok: bool,
    },
    /// A form that the server does not run yet, with the text of the error.
    Unsupported(&'static str),
}

/// An error of the grammar, with the place in the statement.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(in crate::session) struct Invalid {
    sqlstate: &'static str,
    message: String,
    hint: Option<&'static str>,
    at: usize,
}

/// A statement as the reader gives it: the notices of the scanner, and the statement or the
/// error of the grammar.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(in crate::session) struct Parsed {
    notices: Vec<String>,
    statement: Result<Statement, Invalid>,
}

/// Reads a statement. `None` when it is not a statement on the roles, which leaves it to the
/// engine.
pub(in crate::session) fn parse(sql: &str) -> Option<Parsed> {
    // Most statements that start with these words are on tables, so the second word decides
    // before the whole statement is read.
    let rest = sql.trim_start();
    let rest = rest[rest.find(|c: char| !c.is_ascii_alphabetic())?..].trim_start();
    let second: String =
        rest.chars().take_while(char::is_ascii_alphabetic).collect::<String>().to_ascii_lowercase();
    if !["role", "user", "group"].contains(&second.as_str()) && !rest.starts_with(['-', '/']) {
        return None;
    }
    let (mut tokens, starts) = spanned(sql)?;
    while tokens.last() == Some(&Token::Punct(';')) {
        tokens.pop();
    }
    let verb = match tokens.first() {
        Some(t) if t.is("create") => Verb::Create,
        Some(t) if t.is("alter") => Verb::Alter,
        Some(t) if t.is("drop") => Verb::Drop,
        _ => return None,
    };
    let kind = match tokens.get(1) {
        Some(t) if t.is("role") => Kind::Role,
        Some(t) if t.is("user") => Kind::User,
        Some(t) if t.is("group") => Kind::Group,
        _ => return None,
    };
    // `CREATE USER MAPPING`, `ALTER USER MAPPING` and `DROP USER MAPPING`.
    if kind == Kind::User && tokens.get(2).is_some_and(|t| t.is("mapping")) {
        return None;
    }
    let mut notices = Vec::new();
    for token in &mut tokens {
        if let Token::Word { text, .. } = token
            && text.len() > NAME_LIMIT
        {
            let mut end = NAME_LIMIT;
            while !text.is_char_boundary(end) {
                end -= 1;
            }
            notices
                .push(format!("identifier \"{text}\" will be truncated to \"{}\"", &text[..end]));
            text.truncate(end);
        }
    }
    let mut p = Parser { sql, tokens, starts, at: 2 };
    let statement = match verb {
        Verb::Create => p.create(kind),
        Verb::Alter => p.alter(kind),
        Verb::Drop => p.drop(),
    }
    .and_then(|statement| if p.done() { Ok(statement) } else { Err(p.syntax()) });
    Some(Parsed { notices, statement })
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Verb {
    Create,
    Alter,
    Drop,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Kind {
    Role,
    User,
    Group,
}

struct Parser<'a> {
    sql: &'a str,
    tokens: Vec<Token>,
    starts: Vec<usize>,
    at: usize,
}

impl Parser<'_> {
    fn peek(&self, ahead: usize) -> Option<&Token> {
        self.tokens.get(self.at + ahead)
    }

    fn done(&self) -> bool {
        self.at >= self.tokens.len()
    }

    /// The place of the next token, or the end of the statement.
    fn here(&self) -> usize {
        self.starts.get(self.at).copied().unwrap_or(self.sql.trim_end().len())
    }

    fn eat(&mut self, word: &str) -> bool {
        let next = self.peek(0).is_some_and(|t| t.is(word));
        if next {
            self.at += 1;
        }
        next
    }

    fn expect(&mut self, word: &str) -> Result<(), Invalid> {
        if self.eat(word) { Ok(()) } else { Err(self.syntax()) }
    }

    /// The error of the grammar at the next token.
    fn syntax(&self) -> Invalid {
        let at = self.here();
        let message = match self.starts.get(self.at) {
            None => "syntax error at end of input".to_owned(),
            Some(&start) => {
                let end = self.starts.get(self.at + 1).copied().unwrap_or(self.sql.len());
                let text = self.sql[start..end].trim_end().trim_end_matches(';').trim_end();
                format!("syntax error at or near \"{text}\"")
            }
        };
        Invalid { sqlstate: "42601", message, hint: None, at }
    }

    fn string(&mut self) -> Result<String, Invalid> {
        match self.peek(0) {
            Some(Token::String(text)) => {
                let text = text.clone();
                self.at += 1;
                Ok(text)
            }
            _ => Err(self.syntax()),
        }
    }

    /// `RoleSpec`.
    fn spec(&mut self) -> Result<Spec, Invalid> {
        let at = self.here();
        let spec = match self.peek(0) {
            Some(Token::Word { text, quoted }) => {
                let reserved = !quoted && RESERVED.contains(&text.as_str());
                match (text.as_str(), quoted) {
                    ("current_role", false) => Spec::CurrentRole,
                    ("current_user", false) => Spec::CurrentUser,
                    ("session_user", false) => Spec::SessionUser,
                    _ if reserved => return Err(self.syntax()),
                    ("public", _) => Spec::Public,
                    ("none", _) => {
                        return Err(Invalid {
                            sqlstate: "42939",
                            message: "role name \"none\" is reserved".to_owned(),
                            hint: None,
                            at,
                        });
                    }
                    _ => Spec::Name(text.clone()),
                }
            }
            _ => return Err(self.syntax()),
        };
        self.at += 1;
        Ok(spec)
    }

    /// `RoleId`: a `RoleSpec` that is a name.
    fn id(&mut self) -> Result<String, Invalid> {
        let at = self.here();
        let reserved = |message: String| Invalid { sqlstate: "42939", message, hint: None, at };
        let special = |name: &str| reserved(format!("{name} cannot be used as a role name here"));
        match self.spec()? {
            Spec::Name(name) => Ok(name),
            Spec::Public => Err(reserved("role name \"public\" is reserved".to_owned())),
            Spec::SessionUser => Err(special("SESSION_USER")),
            Spec::CurrentUser => Err(special("CURRENT_USER")),
            Spec::CurrentRole => Err(special("CURRENT_ROLE")),
        }
    }

    /// `role_list`.
    fn specs(&mut self) -> Result<Vec<Spec>, Invalid> {
        let mut specs = vec![self.spec()?];
        while self.peek(0) == Some(&Token::Punct(',')) {
            self.at += 1;
            specs.push(self.spec()?);
        }
        Ok(specs)
    }

    /// `CREATE ROLE|USER|GROUP RoleId [WITH] OptRoleList`.
    fn create(&mut self, kind: Kind) -> Result<Statement, Invalid> {
        let name = self.id()?;
        self.eat("with");
        let mut options = Vec::new();
        while !self.done() {
            options.push(self.option(true)?);
        }
        Ok(Statement::Create { name, user: kind == Kind::User, options })
    }

    /// The forms of `ALTER ROLE`, `ALTER USER` and `ALTER GROUP`.
    fn alter(&mut self, kind: Kind) -> Result<Statement, Invalid> {
        if self.eat("all") {
            return self.alter_set();
        }
        let start = self.at;
        let role = self.spec()?;
        if self.eat("rename") {
            self.at = start;
            let from = self.id()?;
            self.expect("rename")?;
            self.expect("to")?;
            let to = self.id()?;
            return Ok(Statement::Rename { from, to });
        }
        if kind == Kind::Group {
            if !self.eat("add") && !self.eat("drop") {
                return Err(self.syntax());
            }
            self.expect("user")?;
            self.specs()?;
            return Ok(Statement::Unsupported("ALTER GROUP is not supported yet"));
        }
        if self.peek(0).is_some_and(|t| t.is("in") || t.is("set") || t.is("reset")) {
            return self.alter_set();
        }
        self.eat("with");
        let mut options = Vec::new();
        while !self.done() {
            options.push(self.option(false)?);
        }
        Ok(Statement::Alter { role, options })
    }

    /// `ALTER ROLE ... [IN DATABASE name] SET|RESET ...`. The server does not keep settings for
    /// roles yet, so the rest of the statement is not read.
    fn alter_set(&mut self) -> Result<Statement, Invalid> {
        if self.eat("in") {
            self.expect("database")?;
            self.at += 1;
        }
        if !self.eat("set") && !self.eat("reset") {
            return Err(self.syntax());
        }
        self.at = self.tokens.len();
        Ok(Statement::Unsupported("ALTER ROLE SET is not supported yet"))
    }

    /// `DROP ROLE|USER|GROUP [IF EXISTS] role_list`.
    fn drop(&mut self) -> Result<Statement, Invalid> {
        let missing_ok = self.eat("if");
        if missing_ok {
            self.expect("exists")?;
        }
        Ok(Statement::Drop { roles: self.specs()?, missing_ok })
    }

    /// `CreateOptRoleElem` when `create` is true, else `AlterOptRoleElem`.
    fn option(&mut self, create: bool) -> Result<Opt, Invalid> {
        let at = self.here();
        let opt = |value| Ok(Opt { value, at });
        if self.eat("password") {
            if self.eat("null") {
                return opt(Value::Password(None));
            }
            return opt(Value::Password(Some(self.string()?)));
        }
        if self.eat("encrypted") {
            self.expect("password")?;
            return opt(Value::Password(Some(self.string()?)));
        }
        if self.eat("unencrypted") {
            self.expect("password")?;
            self.string()?;
            return Err(Invalid {
                sqlstate: "0A000",
                message: "UNENCRYPTED PASSWORD is no longer supported".to_owned(),
                hint: Some("Remove UNENCRYPTED to store the password in encrypted form instead."),
                at,
            });
        }
        if self.eat("inherit") {
            return opt(Value::Attribute(Attribute::Inherit, true));
        }
        if self.eat("connection") {
            self.expect("limit")?;
            let negative = self.peek(0) == Some(&Token::Punct('-'));
            if matches!(self.peek(0), Some(Token::Punct('-' | '+'))) {
                self.at += 1;
            }
            let Some(Token::Number { text, integer: true }) = self.peek(0) else {
                return Err(self.syntax());
            };
            let Ok(value) = text.parse::<i32>() else { return Err(self.syntax()) };
            self.at += 1;
            return opt(Value::ConnectionLimit(if negative { -value } else { value }));
        }
        if self.eat("valid") {
            self.expect("until")?;
            return opt(Value::ValidUntil(self.string()?));
        }
        if self.eat("user") {
            self.specs()?;
            return opt(Value::Members("rolemembers"));
        }
        if create {
            if self.eat("sysid") {
                match self.peek(0) {
                    Some(Token::Number { integer: true, .. }) => self.at += 1,
                    _ => return Err(self.syntax()),
                }
                return opt(Value::Sysid);
            }
            if self.eat("admin") {
                self.specs()?;
                return opt(Value::Members("adminmembers"));
            }
            if self.eat("role") {
                self.specs()?;
                return opt(Value::Members("rolemembers"));
            }
            if self.eat("in") {
                if !self.eat("role") && !self.eat("group") {
                    return Err(self.syntax());
                }
                self.specs()?;
                return opt(Value::Members("addroleto"));
            }
        }
        let Some(Token::Word { text, quoted }) = self.peek(0) else {
            return Err(self.syntax());
        };
        if !quoted && (RESERVED.contains(&text.as_str()) || OPTION_WORDS.contains(&text.as_str())) {
            return Err(self.syntax());
        }
        let value = match text.as_str() {
            "superuser" => Value::Attribute(Attribute::Superuser, true),
            "nosuperuser" => Value::Attribute(Attribute::Superuser, false),
            "createrole" => Value::Attribute(Attribute::CreateRole, true),
            "nocreaterole" => Value::Attribute(Attribute::CreateRole, false),
            "replication" => Value::Attribute(Attribute::Replication, true),
            "noreplication" => Value::Attribute(Attribute::Replication, false),
            "createdb" => Value::Attribute(Attribute::CreateDb, true),
            "nocreatedb" => Value::Attribute(Attribute::CreateDb, false),
            "login" => Value::Attribute(Attribute::Login, true),
            "nologin" => Value::Attribute(Attribute::Login, false),
            "bypassrls" => Value::Attribute(Attribute::BypassRls, true),
            "nobypassrls" => Value::Attribute(Attribute::BypassRls, false),
            "noinherit" => Value::Attribute(Attribute::Inherit, false),
            other => {
                return Err(Invalid {
                    sqlstate: "42601",
                    message: format!("unrecognized role option \"{other}\""),
                    hint: None,
                    at,
                });
            }
        };
        self.at += 1;
        opt(value)
    }
}

/// What a statement on the roles needs from the session.
pub(in crate::session) struct Context<'a> {
    pub(in crate::session) roles: &'a Roles,
    /// The current user, `GetUserId`.
    pub(in crate::session) current: u32,
    /// The session user, `GetSessionUserId`.
    pub(in crate::session) session: u32,
    pub(in crate::session) guc: &'a Settings,
    pub(in crate::session) datetime: DateTimeInput<'a>,
}

fn failure(sqlstate: &str, message: impl Into<String>) -> Failure {
    Failure { sqlstate: sqlstate.to_owned(), message: message.into(), fields: None, position: None }
}

fn with_detail(sqlstate: &str, message: &str, detail: String) -> Failure {
    let mut fields = Fields::default();
    fields.detail = Some(detail);
    Failure { fields: Some(Box::new(fields)), ..failure(sqlstate, message) }
}

/// A `NOTICE` or a `WARNING` to the client.
fn notice(out: &mut OutBuf, severity: &str, sqlstate: &str, message: &str, more: &[(u8, &str)]) {
    let mut fields: Vec<(u8, &[u8])> = vec![
        (b'S', severity.as_bytes()),
        (b'V', severity.as_bytes()),
        (b'C', sqlstate.as_bytes()),
        (b'M', message.as_bytes()),
    ];
    fields.extend(more.iter().map(|(code, text)| (*code, text.as_bytes())));
    out.notice_response(&fields);
}

/// Runs a statement. `offset` is the place of the statement in the query, for the position of an
/// error.
pub(in crate::session) fn execute(
    parsed: &Parsed,
    offset: usize,
    cx: &Context<'_>,
    out: &mut OutBuf,
) -> Result<CommandTag, Failure> {
    for text in &parsed.notices {
        notice(out, "NOTICE", "42622", text, &[]);
    }
    let statement = match &parsed.statement {
        Ok(statement) => statement,
        Err(invalid) => {
            let mut fields = Fields::default();
            fields.hint = invalid.hint.map(str::to_owned);
            return Err(Failure {
                sqlstate: invalid.sqlstate.to_owned(),
                message: invalid.message.clone(),
                fields: invalid.hint.is_some().then(|| Box::new(fields)),
                position: Some(offset + invalid.at),
            });
        }
    };
    let written = |e: String| failure("XX000", e);
    match statement {
        Statement::Create { name, user, options } => cx
            .roles
            .change(|catalog| create(catalog, name, *user, options, offset, cx, out), written),
        Statement::Alter { role, options } => {
            cx.roles.change(|catalog| alter(catalog, role, options, offset, cx, out), written)
        }
        Statement::Rename { from, to } => {
            cx.roles.change(|catalog| rename(catalog, from, to, cx, out), written)
        }
        Statement::Drop { roles, missing_ok } => {
            cx.roles.change(|catalog| drop(catalog, roles, *missing_ok, cx, out), written)
        }
        Statement::Unsupported(message) => Err(failure("0A000", *message)),
    }
}

/// The options after the check that none is given twice, with the `NOTICE` for `SYSID`.
#[derive(Default)]
struct Options<'a> {
    password: Option<&'a Option<String>>,
    attributes: Vec<(Attribute, bool)>,
    connection_limit: Option<i32>,
    valid_until: Option<&'a str>,
    members: bool,
}

impl Options<'_> {
    fn get(&self, attribute: Attribute) -> Option<bool> {
        self.attributes.iter().find(|(a, _)| *a == attribute).map(|(_, on)| *on)
    }
}

fn options<'a>(list: &'a [Opt], offset: usize, out: &mut OutBuf) -> Result<Options<'a>, Failure> {
    let mut seen: Vec<&'static str> = Vec::new();
    let mut found = Options::default();
    for opt in list {
        if opt.value == Value::Sysid {
            notice(out, "NOTICE", "00000", "SYSID can no longer be specified", &[]);
            continue;
        }
        if seen.contains(&opt.key()) {
            return Err(Failure {
                position: Some(offset + opt.at),
                ..failure("42601", "conflicting or redundant options")
            });
        }
        seen.push(opt.key());
        match &opt.value {
            Value::Password(password) => found.password = Some(password),
            Value::Attribute(attribute, on) => found.attributes.push((*attribute, *on)),
            Value::ConnectionLimit(limit) => found.connection_limit = Some(*limit),
            Value::ValidUntil(text) => found.valid_until = Some(text),
            Value::Members(_) => found.members = true,
            Value::Sysid => {}
        }
    }
    if let Some(limit) = found.connection_limit
        && limit < -1
    {
        return Err(failure("22023", format!("invalid connection limit: {limit}")));
    }
    Ok(found)
}

fn reserved(name: &str, detail: &str) -> Failure {
    with_detail("42939", &format!("role name \"{name}\" is reserved"), detail.to_owned())
}

fn has_newline(name: &str) -> Result<(), Failure> {
    if name.contains(['\n', '\r']) {
        return Err(failure(
            "22023",
            format!("role name \"{name}\" contains a newline or carriage return character"),
        ));
    }
    Ok(())
}

/// `timestamptz_in` for `VALID UNTIL`.
fn valid_until(text: &str, cx: &Context<'_>) -> Result<i64, Failure> {
    timestamptz_in(text, -1, &cx.datetime).map_err(|e| {
        let mut fields = Fields::default();
        fields.detail = e.detail.clone();
        fields.hint = e.hint.clone();
        Failure {
            sqlstate: e.sqlstate.as_str().to_owned(),
            message: e.message,
            fields: Some(Box::new(fields)),
            position: None,
        }
    })
}

/// The secret to store for a password, as `CreateRole` and `AlterRole` make it with
/// `encrypt_password`. `None` for an empty password, which clears the password.
fn secret(
    name: &str,
    password: &str,
    cx: &Context<'_>,
    out: &mut OutBuf,
) -> Result<Option<String>, Failure> {
    let kind = PasswordType::of(password.as_bytes());
    if password.is_empty()
        || (kind != PasswordType::Plaintext
            && verify_password(&Provider, name.as_bytes(), password.as_bytes(), b""))
    {
        notice(
            out,
            "NOTICE",
            "00000",
            "empty string is not a valid password, clearing password",
            &[],
        );
        return Ok(None);
    }
    let text = |name: &str| cx.guc.get(name).unwrap_or_default();
    let secret = if kind != PasswordType::Plaintext {
        password.to_owned()
    } else if text("password_encryption") == "md5" {
        md5_encrypt(&Provider, password.as_bytes(), name.as_bytes())
    } else {
        scram(password, text("scram_iterations").parse().unwrap_or(4096))
    };
    if secret.len() > SECRET_LIMIT {
        return Err(with_detail(
            "54000",
            "encrypted password is too long",
            format!("Encrypted passwords must be no longer than {SECRET_LIMIT} bytes."),
        ));
    }
    if text("md5_password_warnings") == "on"
        && PasswordType::of(secret.as_bytes()) == PasswordType::Md5
    {
        notice(
            out,
            "WARNING",
            "01P01",
            "setting an MD5-encrypted password",
            &[
                (
                    b'D',
                    "MD5 password support is deprecated and will be removed in a future release of \
                     PostgreSQL.",
                ),
                (
                    b'H',
                    "Refer to the PostgreSQL documentation for details about migrating to another \
                     password type.",
                ),
            ],
        );
    }
    Ok(Some(secret))
}

fn denied(action: &str, detail: String) -> Failure {
    with_detail("42501", &format!("permission denied to {action}"), detail)
}

/// `CreateRole`.
fn create(
    catalog: &mut Catalog,
    name: &str,
    user: bool,
    list: &[Opt],
    offset: usize,
    cx: &Context<'_>,
    out: &mut OutBuf,
) -> Result<CommandTag, Failure> {
    has_newline(name)?;
    let options = options(list, offset, out)?;
    let me = cx.current;
    let current = catalog.by_oid(me).cloned().unwrap_or_else(|| Role::new(me, ""));
    let on = |attribute| options.get(attribute).unwrap_or(false);
    if !current.superuser {
        let create = |what: &str| {
            denied(
                "create role",
                format!(
                    "Only roles with the {what} attribute may create roles with the {what} attribute."
                ),
            )
        };
        if !current.createrole {
            return Err(denied(
                "create role",
                "Only roles with the CREATEROLE attribute may create roles.".to_owned(),
            ));
        }
        if on(Attribute::Superuser) {
            return Err(create("SUPERUSER"));
        }
        if on(Attribute::CreateDb) && !current.createdb {
            return Err(create("CREATEDB"));
        }
        if on(Attribute::Replication) && !current.replication {
            return Err(create("REPLICATION"));
        }
        if on(Attribute::BypassRls) && !current.bypassrls {
            return Err(create("BYPASSRLS"));
        }
    }
    if name.starts_with("pg_") {
        return Err(reserved(name, "Role names starting with \"pg_\" are reserved."));
    }
    if catalog.find(name).is_some() {
        return Err(failure("42710", format!("role \"{name}\" already exists")));
    }
    if options.members {
        return Err(failure("0A000", "role membership options are not supported yet"));
    }
    let mut role = Role::new(catalog.next_oid(), name);
    role.login = user;
    if let Some(text) = options.valid_until {
        role.valid_until = Some(valid_until(text, cx)?);
    }
    for (attribute, value) in &options.attributes {
        set(&mut role, *attribute, *value);
    }
    if let Some(limit) = options.connection_limit {
        role.connlimit = limit;
    }
    if let Some(Some(password)) = options.password {
        role.password = secret(name, password, cx, out)?;
    }
    let oid = role.oid;
    catalog.roles.push(role);
    if !current.superuser {
        // The creator gets the ADMIN option on the new role from the bootstrap superuser, so it
        // cannot revoke it, and `createrole_self_grant` can add a grant of its own.
        catalog.members.push(Member {
            role: oid,
            member: me,
            grantor: BOOTSTRAP_SUPERUSER,
            admin: true,
            inherit: false,
            set: false,
        });
        let grant = cx.guc.get("createrole_self_grant").unwrap_or_default().to_ascii_lowercase();
        let words: Vec<&str> = grant.split(',').map(str::trim).filter(|w| !w.is_empty()).collect();
        if !words.is_empty() {
            catalog.members.push(Member {
                role: oid,
                member: me,
                grantor: me,
                admin: false,
                inherit: words.contains(&"inherit"),
                set: words.contains(&"set"),
            });
        }
    }
    Ok(CommandTag::CreateRole)
}

fn set(role: &mut Role, attribute: Attribute, on: bool) {
    let field = match attribute {
        Attribute::Superuser => &mut role.superuser,
        Attribute::Inherit => &mut role.inherit,
        Attribute::CreateRole => &mut role.createrole,
        Attribute::CreateDb => &mut role.createdb,
        Attribute::Login => &mut role.login,
        Attribute::Replication => &mut role.replication,
        Attribute::BypassRls => &mut role.bypassrls,
    };
    *field = on;
}

/// The OID of a role in a statement, `get_rolespec_oid`.
fn oid_of(catalog: &Catalog, spec: &Spec, cx: &Context<'_>) -> Result<u32, Failure> {
    let missing = |name: &str| failure("42704", format!("role \"{name}\" does not exist"));
    match spec {
        Spec::Name(name) => catalog.find(name).map(|role| role.oid).ok_or_else(|| missing(name)),
        Spec::Public => Err(missing("public")),
        Spec::CurrentRole | Spec::CurrentUser => Ok(cx.current),
        Spec::SessionUser => Ok(cx.session),
    }
}

/// `AlterRole`.
fn alter(
    catalog: &mut Catalog,
    spec: &Spec,
    list: &[Opt],
    offset: usize,
    cx: &Context<'_>,
    out: &mut OutBuf,
) -> Result<CommandTag, Failure> {
    if let Spec::Name(name) = spec
        && name.starts_with("pg_")
    {
        return Err(reserved(name, "Cannot alter reserved roles."));
    }
    let options = options(list, offset, out)?;
    let oid = oid_of(catalog, spec, cx)?;
    let target = catalog
        .by_oid(oid)
        .cloned()
        .ok_or_else(|| failure("XX000", "cache lookup failed for role"))?;
    let me = cx.current;
    let current = catalog.by_oid(me).cloned().unwrap_or_else(|| Role::new(me, ""));
    let alter = |detail: String| denied("alter role", detail);
    let change = |what: &str| {
        alter(format!("Only roles with the {what} attribute may change the {what} attribute."))
    };
    if !current.superuser && target.superuser {
        return Err(alter(
            "Only roles with the SUPERUSER attribute may alter roles with the SUPERUSER attribute."
                .to_owned(),
        ));
    }
    if !current.superuser && options.get(Attribute::Superuser).is_some() {
        return Err(change("SUPERUSER"));
    }
    if !catalog.createrole(me) || !catalog.is_admin(me, oid) {
        let others = options.attributes.iter().any(|(a, _)| *a != Attribute::Superuser)
            || options.connection_limit.is_some()
            || options.valid_until.is_some();
        if others {
            return Err(alter(format!(
                "Only roles with the CREATEROLE attribute and the ADMIN option on role \"{}\" may \
                 alter this role.",
                target.name
            )));
        }
        if options.password.is_some() && oid != me {
            return Err(alter(
                "To change another role's password, the current user must have the CREATEROLE \
                 attribute and the ADMIN option on the role."
                    .to_owned(),
            ));
        }
    } else if !current.superuser {
        if options.get(Attribute::CreateDb).is_some() && !current.createdb {
            return Err(change("CREATEDB"));
        }
        if options.get(Attribute::Replication).is_some() && !current.replication {
            return Err(change("REPLICATION"));
        }
        if options.get(Attribute::BypassRls).is_some() && !current.bypassrls {
            return Err(change("BYPASSRLS"));
        }
    }
    if options.members {
        if !catalog.is_admin(me, oid) {
            return Err(alter(format!(
                "Only roles with the ADMIN option on role \"{}\" may add or drop members.",
                target.name
            )));
        }
        return Err(failure("0A000", "role membership options are not supported yet"));
    }
    let until = match options.valid_until {
        Some(text) => Some(valid_until(text, cx)?),
        None => target.valid_until,
    };
    if options.get(Attribute::Superuser) == Some(false) && oid == BOOTSTRAP_SUPERUSER {
        return Err(with_detail(
            "0A000",
            "permission denied to alter role",
            "The bootstrap superuser must have the SUPERUSER attribute.".to_owned(),
        ));
    }
    let password = match options.password {
        Some(Some(password)) => Some(secret(&target.name, password, cx, out)?),
        Some(None) => Some(None),
        None => None,
    };
    let role =
        catalog.by_oid_mut(oid).ok_or_else(|| failure("XX000", "cache lookup failed for role"))?;
    for (attribute, on) in &options.attributes {
        set(role, *attribute, *on);
    }
    if let Some(limit) = options.connection_limit {
        role.connlimit = limit;
    }
    if let Some(password) = password {
        role.password = password;
    }
    role.valid_until = until;
    Ok(CommandTag::AlterRole)
}

/// `RenameRole`.
fn rename(
    catalog: &mut Catalog,
    from: &str,
    to: &str,
    cx: &Context<'_>,
    out: &mut OutBuf,
) -> Result<CommandTag, Failure> {
    has_newline(to)?;
    let role = catalog
        .find(from)
        .cloned()
        .ok_or_else(|| failure("42704", format!("role \"{from}\" does not exist")))?;
    if role.oid == cx.session {
        return Err(failure("0A000", "session user cannot be renamed"));
    }
    if role.oid == cx.current {
        return Err(failure("0A000", "current user cannot be renamed"));
    }
    for name in [from, to] {
        if name.starts_with("pg_") {
            return Err(reserved(name, "Role names starting with \"pg_\" are reserved."));
        }
    }
    if catalog.find(to).is_some() {
        return Err(failure("42710", format!("role \"{to}\" already exists")));
    }
    let me = cx.current;
    if role.superuser {
        if !catalog.superuser(me) {
            return Err(denied(
                "rename role",
                "Only roles with the SUPERUSER attribute may rename roles with the SUPERUSER \
                 attribute."
                    .to_owned(),
            ));
        }
    } else if !catalog.createrole(me) || !catalog.is_admin(me, role.oid) {
        return Err(denied(
            "rename role",
            format!(
                "Only roles with the CREATEROLE attribute and the ADMIN option on role \"{from}\" \
                 may rename this role."
            ),
        ));
    }
    let target = catalog
        .by_oid_mut(role.oid)
        .ok_or_else(|| failure("XX000", "cache lookup failed for role"))?;
    to.clone_into(&mut target.name);
    // MD5 uses the name as the salt, so the secret is no good after a rename.
    if target
        .password
        .as_deref()
        .is_some_and(|p| PasswordType::of(p.as_bytes()) == PasswordType::Md5)
    {
        target.password = None;
        notice(out, "NOTICE", "00000", "MD5 password cleared because of role rename", &[]);
    }
    Ok(CommandTag::AlterRole)
}

/// `DropRole`.
fn drop(
    catalog: &mut Catalog,
    specs: &[Spec],
    missing_ok: bool,
    cx: &Context<'_>,
    out: &mut OutBuf,
) -> Result<CommandTag, Failure> {
    let me = cx.current;
    if !catalog.createrole(me) {
        return Err(denied(
            "drop role",
            "Only roles with the CREATEROLE attribute and the ADMIN option on the target roles may \
             drop roles."
                .to_owned(),
        ));
    }
    for spec in specs {
        let Spec::Name(name) = spec else {
            return Err(failure("22023", "cannot use special role specifier in DROP ROLE"));
        };
        let Some(role) = catalog.find(name).cloned() else {
            if !missing_ok {
                return Err(failure("42704", format!("role \"{name}\" does not exist")));
            }
            notice(
                out,
                "NOTICE",
                "00000",
                &format!("role \"{name}\" does not exist, skipping"),
                &[],
            );
            continue;
        };
        if role.oid == me {
            return Err(failure("55006", "current user cannot be dropped"));
        }
        if role.oid == cx.session {
            return Err(failure("55006", "session user cannot be dropped"));
        }
        if role.superuser && !catalog.superuser(me) {
            return Err(denied(
                "drop role",
                "Only roles with the SUPERUSER attribute may drop roles with the SUPERUSER \
                 attribute."
                    .to_owned(),
            ));
        }
        if !catalog.is_admin(me, role.oid) {
            return Err(denied(
                "drop role",
                format!(
                    "Only roles with the CREATEROLE attribute and the ADMIN option on role \
                     \"{name}\" may drop this role."
                ),
            ));
        }
        if role.oid == BOOTSTRAP_SUPERUSER {
            return Err(failure(
                "2BP01",
                format!("cannot drop role {name} because it is required by the database system"),
            ));
        }
        catalog.roles.retain(|r| r.oid != role.oid);
        catalog.members.retain(|m| m.role != role.oid && m.member != role.oid);
    }
    Ok(CommandTag::DropRole)
}

#[cfg(test)]
mod tests {
    use super::{Attribute, Invalid, Opt, Spec, Statement, Value, parse};

    fn statement(sql: &str) -> Result<Statement, Invalid> {
        parse(sql).expect("a statement on roles").statement
    }

    fn error(sql: &str) -> (&'static str, String, usize) {
        let invalid = statement(sql).expect_err("an error");
        (invalid.sqlstate, invalid.message, invalid.at)
    }

    #[test]
    fn other_statements_go_to_the_engine() {
        for sql in [
            "create table t (a int)",
            "drop table t",
            "alter table t add b int",
            "create user mapping for x server s",
            "select 1",
        ] {
            assert_eq!(parse(sql), None, "{sql}");
        }
    }

    #[test]
    fn the_forms() {
        let Ok(Statement::Create { name, user, options }) = statement(
            "CREATE USER \"Ann\" WITH LOGIN password 'x' CONNECTION LIMIT -1 valid until 'infinity';",
        ) else {
            panic!("a create");
        };
        assert_eq!((name.as_str(), user), ("Ann", true));
        assert_eq!(
            options.iter().map(|o| o.value.clone()).collect::<Vec<_>>(),
            vec![
                Value::Attribute(Attribute::Login, true),
                Value::Password(Some("x".to_owned())),
                Value::ConnectionLimit(-1),
                Value::ValidUntil("infinity".to_owned()),
            ]
        );
        assert_eq!(
            statement("alter role current_user nosuperuser"),
            Ok(Statement::Alter {
                role: Spec::CurrentUser,
                options: vec![Opt { value: Value::Attribute(Attribute::Superuser, false), at: 24 }],
            })
        );
        assert_eq!(
            statement("alter group a rename to b"),
            Ok(Statement::Rename { from: "a".to_owned(), to: "b".to_owned() })
        );
        assert_eq!(
            statement("drop role if exists a, public"),
            Ok(Statement::Drop {
                roles: vec![Spec::Name("a".to_owned()), Spec::Public],
                missing_ok: true
            })
        );
        assert!(matches!(
            statement("alter role all in database d set x = 1"),
            Ok(Statement::Unsupported(_))
        ));
    }

    #[test]
    fn the_errors_of_the_grammar() {
        assert_eq!(
            error("create role x foo"),
            ("42601", "unrecognized role option \"foo\"".to_owned(), 14)
        );
        assert_eq!(
            error("create role x \"LOGIN\""),
            ("42601", "unrecognized role option \"LOGIN\"".to_owned(), 14)
        );
        assert_eq!(
            error("create role none"),
            ("42939", "role name \"none\" is reserved".to_owned(), 12)
        );
        assert_eq!(
            error("create role public"),
            ("42939", "role name \"public\" is reserved".to_owned(), 12)
        );
        assert_eq!(
            error("create role current_user"),
            ("42939", "CURRENT_USER cannot be used as a role name here".to_owned(), 12)
        );
        assert_eq!(
            error("create role select"),
            ("42601", "syntax error at or near \"select\"".to_owned(), 12)
        );
        assert_eq!(error("create role"), ("42601", "syntax error at end of input".to_owned(), 11));
        assert_eq!(
            error("alter role x sysid 3"),
            ("42601", "syntax error at or near \"sysid\"".to_owned(), 13)
        );
        assert_eq!(
            error("create role x unencrypted password 'a'"),
            ("0A000", "UNENCRYPTED PASSWORD is no longer supported".to_owned(), 14)
        );
        assert_eq!(
            error("create role x connection limit 99999999999"),
            ("42601", "syntax error at or near \"99999999999\"".to_owned(), 31)
        );
        let long = "a".repeat(70);
        let parsed = parse(&format!("create role {long}")).expect("a statement on roles");
        assert_eq!(parsed.notices.len(), 1);
        assert!(matches!(parsed.statement, Ok(Statement::Create { name, .. }) if name.len() == 63));
    }
}
