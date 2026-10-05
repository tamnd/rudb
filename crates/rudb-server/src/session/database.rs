//! `CREATE DATABASE`, `ALTER DATABASE` and `DROP DATABASE`, which the server runs itself on the
//! databases of the cluster.
//!
//! The reader follows the rules of `gram.y` for these statements, and the checks run in the order
//! of `createdb`, `dropdb`, `RenameDatabase`, `AlterDatabase`, `AlterDatabaseOwner` and `movedb`
//! in `dbcommands.c`, so that a statement fails with the same error as in PostgreSQL. rudb keeps
//! text in UTF8 and compares it by bytes, so a database with another encoding or with a locale
//! that sorts in another order fails with `0A000`, after all the checks of PostgreSQL.

use rudb_common::Fields;
use rudb_pgwire::{CommandTag, OutBuf};

use super::Failure;
use super::keywords::{Category, category};
use super::role::{Invalid, Parser, Spec, failure, notice, truncate, with_detail};
use super::setting::{Token, spanned};
use crate::databases::{self, Catalog, Row, UTF8, encoding_name};
use crate::locale::{self, Codeset};
use crate::roles::{self, BOOTSTRAP_SUPERUSER, FIRST_NORMAL_OID};
use crate::server::Shared;

/// The value of an option, as the grammar gives it to the `defGet` functions.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(in crate::session) enum Arg {
    /// A number that fits in an `int4`.
    Integer(i32),
    /// Another number, as written.
    Float(String),
    /// A string, a name or a key word.
    Text(String),
}

/// An option, `DefElem`, with the place where its name starts. `arg` is `None` for `DEFAULT`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(in crate::session) struct Opt {
    name: String,
    arg: Option<Arg>,
    at: usize,
}

/// A statement on the databases.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(in crate::session) enum Statement {
    Create {
        name: String,
        options: Vec<Opt>,
    },
    Drop {
        name: String,
        missing_ok: bool,
        force: bool,
    },
    Rename {
        from: String,
        to: String,
    },
    Owner {
        name: String,
        owner: Spec,
    },
    /// `ALTER DATABASE name [WITH] options`, and `SET TABLESPACE` as the option `tablespace`.
    Alter {
        name: String,
        options: Vec<Opt>,
    },
    Refresh {
        name: String,
    },
    /// A form that the server does not run yet, with the text of the error.
    Unsupported(&'static str),
}

/// A statement as the reader gives it: the notices of the scanner, and the statement or the
/// error of the grammar.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(in crate::session) struct Parsed {
    notices: Vec<String>,
    statement: Result<Statement, Invalid>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Verb {
    Create,
    Alter,
    Drop,
}

/// Reads a statement. `None` when it is not a statement on the databases.
pub(in crate::session) fn parse(sql: &str) -> Option<Parsed> {
    let rest = sql.trim_start();
    let rest = rest[rest.find(|c: char| !c.is_ascii_alphabetic())?..].trim_start();
    let second: String =
        rest.chars().take_while(char::is_ascii_alphabetic).collect::<String>().to_ascii_lowercase();
    if second != "database" && !rest.starts_with(['-', '/']) {
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
    if !tokens.get(1).is_some_and(|t| t.is("database")) {
        return None;
    }
    let notices = truncate(&mut tokens);
    let mut p = Parser { sql, tokens, starts, at: 2 };
    let statement = match verb {
        Verb::Create => create_statement(&mut p),
        Verb::Alter => alter_statement(&mut p),
        Verb::Drop => drop_statement(&mut p),
    }
    .and_then(|statement| if p.done() { Ok(statement) } else { Err(p.syntax()) });
    Some(Parsed { notices, statement })
}

fn is_punct(p: &Parser<'_>, ahead: usize, c: char) -> bool {
    p.peek(ahead) == Some(&Token::Punct(c))
}

/// `name`, which is `ColId`: a name, or a key word that is not reserved and is not a type or a
/// function name.
fn name(p: &mut Parser<'_>) -> Result<String, Invalid> {
    match p.peek(0) {
        Some(Token::Word { text, quoted })
            if *quoted
                || matches!(
                    category(text),
                    None | Some(Category::Unreserved | Category::ColName)
                ) =>
        {
            let text = text.clone();
            p.at += 1;
            Ok(text)
        }
        _ => Err(p.syntax()),
    }
}

/// `createdb_opt_item`: `createdb_opt_name [=] value`.
fn option(p: &mut Parser<'_>) -> Result<Opt, Invalid> {
    let at = p.here();
    let name = match p.peek(0) {
        Some(Token::Word { text, quoted: true }) => text.clone(),
        Some(Token::Word { text, quoted: false }) => match text.as_str() {
            "connection" => {
                if !p.peek(1).is_some_and(|t| t.is("limit")) {
                    p.at += 1;
                    return Err(p.syntax());
                }
                p.at += 1;
                "connection_limit".to_owned()
            }
            "encoding" | "location" | "owner" | "tablespace" | "template" => text.clone(),
            _ if category(text).is_none() => text.clone(),
            _ => return Err(p.syntax()),
        },
        _ => return Err(p.syntax()),
    };
    p.at += 1;
    if is_punct(p, 0, '=') {
        p.at += 1;
    }
    let arg = value(p)?;
    Ok(Opt { name, arg, at })
}

/// The value of an option: `NumericOnly`, `opt_boolean_or_string` or `DEFAULT`.
fn value(p: &mut Parser<'_>) -> Result<Option<Arg>, Invalid> {
    let negative = is_punct(p, 0, '-');
    if negative || is_punct(p, 0, '+') {
        p.at += 1;
        let Some(Token::Number { text, integer }) = p.peek(0) else {
            return Err(p.syntax());
        };
        let arg = number(text, *integer, negative);
        p.at += 1;
        return Ok(Some(arg));
    }
    let arg = match p.peek(0) {
        Some(Token::Number { text, integer }) => number(text, *integer, false),
        Some(Token::String(text) | Token::Word { text, quoted: true }) => Arg::Text(text.clone()),
        Some(Token::Word { text, quoted: false }) => match text.as_str() {
            "default" => {
                p.at += 1;
                return Ok(None);
            }
            "true" | "false" | "on" => Arg::Text(text.clone()),
            _ if category(text) != Some(Category::Reserved) => Arg::Text(text.clone()),
            _ => return Err(p.syntax()),
        },
        _ => return Err(p.syntax()),
    };
    p.at += 1;
    Ok(Some(arg))
}

/// A number as the scanner and `NumericOnly` make it: an integer that fits in an `int4` is an
/// `Integer`, any other number is a `Float` with its text.
fn number(text: &str, integer: bool, negative: bool) -> Arg {
    if integer && let Ok(value) = text.parse::<i32>() {
        return Arg::Integer(if negative { -value } else { value });
    }
    Arg::Float(if negative { format!("-{text}") } else { text.to_owned() })
}

/// `CREATE DATABASE name [WITH] createdb_opt_list`.
fn create_statement(p: &mut Parser<'_>) -> Result<Statement, Invalid> {
    let name = name(p)?;
    p.eat("with");
    let mut options = Vec::new();
    while !p.done() {
        options.push(option(p)?);
    }
    Ok(Statement::Create { name, options })
}

/// The forms of `ALTER DATABASE`.
fn alter_statement(p: &mut Parser<'_>) -> Result<Statement, Invalid> {
    let name = name(p)?;
    if p.eat("rename") {
        p.expect("to")?;
        let to = self::name(p)?;
        return Ok(Statement::Rename { from: name, to });
    }
    if p.peek(0).is_some_and(|t| t.is("owner")) && p.peek(1).is_some_and(|t| t.is("to")) {
        p.at += 2;
        let owner = p.spec()?;
        return Ok(Statement::Owner { name, owner });
    }
    if p.eat("refresh") {
        p.expect("collation")?;
        p.expect("version")?;
        return Ok(Statement::Refresh { name });
    }
    let setting = p.peek(2).is_some_and(|t| t.is("to") || *t == Token::Punct('='));
    if p.peek(0).is_some_and(|t| t.is("set"))
        && p.peek(1).is_some_and(|t| t.is("tablespace"))
        && !setting
    {
        p.at += 2;
        let at = p.here();
        let space = self::name(p)?;
        let options = vec![Opt { name: "tablespace".to_owned(), arg: Some(Arg::Text(space)), at }];
        return Ok(Statement::Alter { name, options });
    }
    // The server does not keep settings for databases yet, so the rest of the statement is not
    // read.
    if p.eat("set") || p.eat("reset") {
        p.at = p.tokens.len();
        return Ok(Statement::Unsupported("ALTER DATABASE SET is not supported yet"));
    }
    p.eat("with");
    let mut options = Vec::new();
    while !p.done() {
        options.push(option(p)?);
    }
    Ok(Statement::Alter { name, options })
}

/// `DROP DATABASE [IF EXISTS] name [[WITH] (FORCE [, ...])]`.
fn drop_statement(p: &mut Parser<'_>) -> Result<Statement, Invalid> {
    let missing_ok =
        p.peek(0).is_some_and(|t| t.is("if")) && p.peek(1).is_some_and(|t| t.is("exists"));
    if missing_ok {
        p.at += 2;
    }
    let name = name(p)?;
    let mut force = false;
    if !p.done() {
        p.eat("with");
        if !is_punct(p, 0, '(') {
            return Err(p.syntax());
        }
        p.at += 1;
        loop {
            p.expect("force")?;
            force = true;
            if !is_punct(p, 0, ',') {
                break;
            }
            p.at += 1;
        }
        if !is_punct(p, 0, ')') {
            return Err(p.syntax());
        }
        p.at += 1;
    }
    Ok(Statement::Drop { name, missing_ok, force })
}

/// What a statement on the databases needs from the session.
pub(in crate::session) struct Context<'a> {
    pub(in crate::session) shared: &'a Shared,
    /// The process ID of the session.
    pub(in crate::session) pid: i32,
    /// The database of the session, `MyDatabaseId`.
    pub(in crate::session) database: u32,
    /// The current user, `GetUserId`.
    pub(in crate::session) current: u32,
    /// The session user, `GetSessionUserId`.
    pub(in crate::session) session: u32,
    /// True in a transaction block, where `PreventInTransactionBlock` fails.
    pub(in crate::session) block: bool,
    /// The text that the positions count in, for the position of a `WARNING`.
    pub(in crate::session) sql: &'a str,
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
    // One statement on the databases at a time, as the locks of PostgreSQL order them.
    let _ddl = cx.shared.ddl();
    match statement {
        Statement::Create { name, options } => {
            prevent(cx, "CREATE DATABASE")?;
            create(name, options, offset, cx, out)?;
            Ok(CommandTag::CreateDatabase)
        }
        Statement::Drop { name, missing_ok, force } => {
            prevent(cx, "DROP DATABASE")?;
            drop(name, *missing_ok, *force, cx, out)?;
            Ok(CommandTag::DropDatabase)
        }
        Statement::Rename { from, to } => {
            rename(from, to, cx)?;
            Ok(CommandTag::AlterDatabase)
        }
        Statement::Owner { name, owner } => {
            set_owner(name, owner, cx)?;
            Ok(CommandTag::AlterDatabase)
        }
        Statement::Alter { name, options } => {
            alter(name, options, offset, cx)?;
            Ok(CommandTag::AlterDatabase)
        }
        Statement::Refresh { name } => {
            refresh(name, cx, out)?;
            Ok(CommandTag::AlterDatabase)
        }
        Statement::Unsupported(message) => Err(failure("0A000", *message)),
    }
}

/// `PreventInTransactionBlock`.
fn prevent(cx: &Context<'_>, what: &str) -> Result<(), Failure> {
    if cx.block {
        return Err(failure("25001", format!("{what} cannot run inside a transaction block")));
    }
    Ok(())
}

fn with_hint(sqlstate: &str, message: impl Into<String>, hint: &str) -> Failure {
    let mut fields = Fields::default();
    fields.hint = Some(hint.to_owned());
    Failure { fields: Some(Box::new(fields)), ..failure(sqlstate, message) }
}

fn at(failure: Failure, offset: usize, opt: &Opt) -> Failure {
    Failure { position: Some(offset + opt.at), ..failure }
}

fn has_newline(name: &str) -> Result<(), Failure> {
    if name.contains(['\n', '\r']) {
        return Err(failure(
            "22023",
            format!("database name \"{name}\" contains a newline or carriage return character"),
        ));
    }
    Ok(())
}

/// `defGetString`.
fn string(opt: &Opt) -> Result<String, Failure> {
    match &opt.arg {
        None => Err(failure("42601", format!("{} requires a parameter", opt.name))),
        Some(Arg::Integer(value)) => Ok(value.to_string()),
        Some(Arg::Float(text) | Arg::Text(text)) => Ok(text.clone()),
    }
}

/// `defGetInt32`.
fn int32(opt: &Opt) -> Result<i32, Failure> {
    match &opt.arg {
        None => Err(failure("42601", format!("{} requires a parameter", opt.name))),
        Some(Arg::Integer(value)) => Ok(*value),
        Some(_) => Err(failure("42601", format!("{} requires an integer value", opt.name))),
    }
}

/// `defGetBoolean`.
fn boolean(opt: &Opt) -> Result<bool, Failure> {
    let wrong = || failure("42601", format!("{} requires a Boolean value", opt.name));
    match &opt.arg {
        None => Ok(true),
        Some(Arg::Integer(0)) => Ok(false),
        Some(Arg::Integer(1)) => Ok(true),
        Some(Arg::Integer(_) | Arg::Float(_)) => Err(wrong()),
        Some(Arg::Text(text)) => match text.to_ascii_lowercase().as_str() {
            "true" | "on" => Ok(true),
            "false" | "off" => Ok(false),
            _ => Err(wrong()),
        },
    }
}

/// `defGetObjectId`.
fn object_id(opt: &Opt) -> Result<u32, Failure> {
    match &opt.arg {
        None => Err(failure("42601", format!("{} requires a parameter", opt.name))),
        Some(Arg::Integer(value)) => Ok(value.cast_unsigned()),
        Some(Arg::Float(text)) => match text.parse::<i64>() {
            Ok(value) if (i64::from(i32::MIN)..=i64::from(u32::MAX)).contains(&value) => {
                Ok(value as u32)
            }
            Ok(_) => {
                Err(failure("22003", format!("value \"{text}\" is out of range for type oid")))
            }
            Err(_) => {
                Err(failure("22P02", format!("invalid input syntax for type oid: \"{text}\"")))
            }
        },
        Some(Arg::Text(_)) => {
            Err(failure("42601", format!("{} requires a numeric value", opt.name)))
        }
    }
}

/// The error of `aclcheck_error` for a database that the user does not own.
fn not_owner(name: &str) -> Failure {
    failure("42501", format!("must be owner of database {name}"))
}

fn missing(name: &str) -> Failure {
    failure("3D000", format!("database \"{name}\" does not exist"))
}

/// `have_createdb_privilege`.
fn createdb(roles: &roles::Catalog, user: u32) -> bool {
    roles.by_oid(user).is_some_and(|role| role.superuser || role.createdb)
}

/// `CountOtherDBBackends` and `errdetail_busy_db`: waits up to five seconds for the other
/// sessions on the database to end, then fails with `message` when some are left.
fn busy(cx: &Context<'_>, oid: u32, message: String) -> Result<(), Failure> {
    let others = cx.shared.wait_others(oid, cx.pid);
    if others == 0 {
        return Ok(());
    }
    let detail = if others == 1 {
        "There is 1 other session using the database.".to_owned()
    } else {
        format!("There are {others} other sessions using the database.")
    };
    Err(with_detail("55006", &message, detail))
}

fn written(error: String) -> Failure {
    failure("XX000", error)
}

/// The names of the options of `CREATE DATABASE` that can be given once.
const CREATE_OPTIONS: [&str; 16] = [
    "tablespace",
    "owner",
    "template",
    "encoding",
    "locale",
    "builtin_locale",
    "lc_collate",
    "lc_ctype",
    "icu_locale",
    "icu_rules",
    "locale_provider",
    "is_template",
    "allow_connections",
    "connection_limit",
    "collation_version",
    "strategy",
];

/// The options of a statement after the check that none is given twice.
struct Found<'a>(Vec<&'a Opt>);

impl<'a> Found<'a> {
    fn get(&self, name: &str) -> Option<&'a Opt> {
        self.0.iter().copied().find(|opt| opt.name == name)
    }

    /// The option when it has a value, which is not `DEFAULT`.
    fn arg(&self, name: &str) -> Option<&'a Opt> {
        self.get(name).filter(|opt| opt.arg.is_some())
    }
}

fn conflicting(opt: &Opt, offset: usize) -> Failure {
    at(failure("42601", "conflicting or redundant options"), offset, opt)
}

/// The name of a locale provider, `collprovider_name`.
fn provider_name(provider: char) -> &'static str {
    match provider {
        'b' => "builtin",
        'i' => "icu",
        _ => "libc",
    }
}

/// `builtin_validate_locale`.
fn builtin_locale(encoding: i32, name: &str) -> Result<&'static str, Failure> {
    let (canonical, utf8) = match name {
        "C" => ("C", false),
        "C.UTF-8" | "C.UTF8" => ("C.UTF-8", true),
        "PG_UNICODE_FAST" => ("PG_UNICODE_FAST", true),
        _ => {
            return Err(failure(
                "42809",
                format!("invalid locale name \"{name}\" for builtin provider"),
            ));
        }
    };
    if utf8 && encoding != UTF8 {
        return Err(failure(
            "42809",
            format!("encoding \"{}\" does not match locale \"{name}\"", encoding_name(encoding)),
        ));
    }
    Ok(canonical)
}

/// `get_collation_actual_version`. The C library of the system gives no version for its locales
/// to rudb, which takes only the locales that compare by bytes.
fn actual_version(provider: char) -> Option<String> {
    (provider == 'b').then(|| "1".to_owned())
}

/// `pg_get_encoding_from_locale` with its warning: the name of the encoding, or `None` when any
/// encoding goes with the locale.
fn locale_encoding(name: &str, out: &mut OutBuf) -> Option<&'static str> {
    match locale::encoding(name) {
        Codeset::Any => None,
        Codeset::Encoding(encoding) => Some(encoding),
        Codeset::Unknown(codeset) => {
            let message = format!(
                "could not determine encoding for locale \"{name}\": codeset is \"{codeset}\""
            );
            notice(out, "WARNING", "01000", &message, &[]);
            None
        }
    }
}

/// `check_encoding_locale_matches`.
fn encoding_matches(
    encoding: i32,
    collate: &str,
    ctype: &str,
    superuser: bool,
    out: &mut OutBuf,
) -> Result<(), Failure> {
    let name = encoding_name(encoding);
    let ctype_encoding = locale_encoding(ctype, out);
    let collate_encoding = locale_encoding(collate, out);
    for (found, locale, setting) in
        [(ctype_encoding, ctype, "LC_CTYPE"), (collate_encoding, collate, "LC_COLLATE")]
    {
        if let Some(found) = found
            && found != name
            && found != "SQL_ASCII"
            && !(name == "SQL_ASCII" && superuser)
        {
            return Err(with_detail(
                "22023",
                &format!("encoding \"{name}\" does not match locale \"{locale}\""),
                format!("The chosen {setting} setting requires encoding \"{found}\"."),
            ));
        }
    }
    Ok(())
}

/// `check_locale` with the errors of `createdb`.
fn check_locale(category: locale::Category, name: &str, provider: char) -> Result<String, Failure> {
    if let Some(canonical) = locale::check(category, name) {
        return Ok(canonical);
    }
    let setting = if category == locale::Category::Collate { "LC_COLLATE" } else { "LC_CTYPE" };
    let message = format!("invalid {setting} locale name: \"{name}\"");
    Err(match provider {
        'b' => with_hint(
            "42809",
            message,
            "If the locale name is specific to the builtin provider, use BUILTIN_LOCALE.",
        ),
        'i' => with_hint(
            "42809",
            message,
            "If the locale name is specific to the ICU provider, use ICU_LOCALE.",
        ),
        _ => failure("42809", message),
    })
}

/// The check of rudb after the checks of PostgreSQL: rudb keeps text in UTF8 and compares it by
/// bytes.
fn supported(row: &Row) -> Result<(), Failure> {
    if row.encoding != UTF8 {
        return Err(with_hint(
            "0A000",
            format!("encoding \"{}\" is not supported", encoding_name(row.encoding)),
            "Use the encoding UTF8.",
        ));
    }
    let builtin = row.locale.as_deref().filter(|_| row.provider == 'b');
    for name in
        [Some(row.collate.as_str()), Some(row.ctype.as_str()), builtin].into_iter().flatten()
    {
        if name == "PG_UNICODE_FAST" || !locale::bytewise(name) {
            let mut fields = Fields::default();
            fields.detail = Some("rudb compares text by bytes, as the C locale does.".to_owned());
            fields.hint = Some("Use the locale C or C.UTF-8.".to_owned());
            return Err(Failure {
                fields: Some(Box::new(fields)),
                ..failure("0A000", format!("locale \"{name}\" is not supported"))
            });
        }
    }
    Ok(())
}

/// `createdb`.
#[allow(clippy::too_many_lines)]
fn create(
    name: &str,
    list: &[Opt],
    offset: usize,
    cx: &Context<'_>,
    out: &mut OutBuf,
) -> Result<(), Failure> {
    has_newline(name)?;
    let mut found = Found(Vec::new());
    let mut oid = None;
    for opt in list {
        match opt.name.as_str() {
            key if CREATE_OPTIONS.contains(&key) => {
                if found.get(key).is_some() {
                    return Err(conflicting(opt, offset));
                }
                found.0.push(opt);
            }
            "location" => {
                let position = super::position(cx.sql, offset + opt.at);
                notice(
                    out,
                    "WARNING",
                    "0A000",
                    "LOCATION is not supported anymore",
                    &[(b'H', "Consider using tablespaces instead."), (b'P', &position)],
                );
            }
            "oid" => {
                let value = object_id(opt)?;
                if value < FIRST_NORMAL_OID {
                    return Err(failure(
                        "22023",
                        format!(
                            "OIDs less than {FIRST_NORMAL_OID} are reserved for system objects"
                        ),
                    ));
                }
                oid = Some(value);
            }
            other => {
                return Err(at(
                    failure("42601", format!("option \"{other}\" not recognized")),
                    offset,
                    opt,
                ));
            }
        }
    }
    let text = |key: &str| found.arg(key).map(string).transpose();
    let owner = text("owner")?;
    let template = text("template")?;
    let mut encoding = None;
    if let Some(opt) = found.arg("encoding") {
        encoding = Some(match &opt.arg {
            Some(Arg::Integer(code)) => {
                if encoding_name(*code).is_empty() {
                    let message = format!("{code} is not a valid encoding code");
                    return Err(at(failure("42704", message), offset, opt));
                }
                *code
            }
            _ => {
                let text = string(opt)?;
                let Some(code) = databases::encoding_code(&text) else {
                    let message = format!("{text} is not a valid encoding name");
                    return Err(at(failure("42704", message), offset, opt));
                };
                code
            }
        });
    }
    let (mut collate, mut ctype, mut locale) = (None, None, None);
    if let Some(value) = text("locale")? {
        (collate, ctype, locale) = (Some(value.clone()), Some(value.clone()), Some(value));
    }
    if let Some(value) = text("builtin_locale")? {
        locale = Some(value);
    }
    if let Some(value) = text("lc_collate")? {
        collate = Some(value);
    }
    if let Some(value) = text("lc_ctype")? {
        ctype = Some(value);
    }
    if let Some(value) = text("icu_locale")? {
        locale = Some(value);
    }
    let icu_rules = text("icu_rules")?;
    let provider = match text("locale_provider")? {
        None => None,
        Some(value) => Some(match value.to_ascii_lowercase().as_str() {
            "builtin" => 'b',
            "icu" => 'i',
            "libc" => 'c',
            _ => {
                return Err(failure("42P17", format!("unrecognized locale provider: {value}")));
            }
        }),
    };
    let template_flag = found.arg("is_template").map(boolean).transpose()?.unwrap_or(false);
    let allow = found.arg("allow_connections").map(boolean).transpose()?.unwrap_or(true);
    let connlimit = found.arg("connection_limit").map(int32).transpose()?.unwrap_or(-1);
    if connlimit < -1 {
        return Err(failure("22023", format!("invalid connection limit: {connlimit}")));
    }
    let collversion = found.get("collation_version").map(string).transpose()?;

    let roles = cx.shared.roles.snapshot();
    let owner = match owner {
        Some(owner) => match roles.find(&owner) {
            Some(role) => role.oid,
            None => return Err(failure("42704", format!("role \"{owner}\" does not exist"))),
        },
        None => cx.current,
    };
    if !createdb(&roles, cx.current) {
        return Err(failure("42501", "permission denied to create database"));
    }
    if !roles.can_set(cx.current, owner) {
        let name = roles.by_oid(owner).map(|role| role.name.as_str()).unwrap_or_default();
        return Err(failure("42501", format!("must be able to SET ROLE \"{name}\"")));
    }
    let template = template.unwrap_or_else(|| "template1".to_owned());
    let catalog = cx.shared.databases.snapshot();
    let Some(source) = catalog.find(&template).cloned() else {
        return Err(failure("3D000", format!("template database \"{template}\" does not exist")));
    };
    if !source.template && !roles.has_privs(cx.current, source.owner) {
        return Err(failure("42501", format!("permission denied to copy database \"{template}\"")));
    }
    if let Some(strategy) = text("strategy")? {
        let lower = strategy.to_ascii_lowercase();
        if lower != "wal_log" && lower != "file_copy" {
            return Err(with_hint(
                "22023",
                format!("invalid create database strategy \"{strategy}\""),
                "Valid strategies are \"wal_log\" and \"file_copy\".",
            ));
        }
    }
    let encoding = encoding.unwrap_or(source.encoding);
    let collate = collate.unwrap_or_else(|| source.collate.clone());
    let ctype = ctype.unwrap_or_else(|| source.ctype.clone());
    let provider = provider.unwrap_or(source.provider);
    let locale = locale.or_else(|| source.locale.clone().filter(|_| provider == source.provider));
    let icu_rules = icu_rules.or_else(|| source.icu_rules.clone());
    let collate = check_locale(locale::Category::Collate, &collate, provider)?;
    let ctype = check_locale(locale::Category::Ctype, &ctype, provider)?;
    encoding_matches(encoding, &collate, &ctype, roles.superuser(cx.current), out)?;
    if provider != 'b' && found.get("builtin_locale").is_some() {
        return Err(failure(
            "42P17",
            "BUILTIN_LOCALE cannot be specified unless locale provider is builtin",
        ));
    }
    if provider != 'i' {
        if found.get("icu_locale").is_some() {
            return Err(failure(
                "42P17",
                "ICU locale cannot be specified unless locale provider is ICU",
            ));
        }
        if icu_rules.is_some() {
            return Err(failure(
                "42P17",
                "ICU rules cannot be specified unless locale provider is ICU",
            ));
        }
    }
    let locale = match provider {
        'b' => {
            let Some(locale) = locale else {
                return Err(failure("22023", "LOCALE or BUILTIN_LOCALE must be specified"));
            };
            Some(builtin_locale(encoding, &locale)?.to_owned())
        }
        'i' => {
            // The ICU encodings of `pg_enc2icu_tbl`: every server encoding but these.
            if ["SQL_ASCII", "EUC_JIS_2004", "LATIN10", "WIN874"].contains(&encoding_name(encoding))
            {
                return Err(failure(
                    "22023",
                    format!(
                        "encoding \"{}\" is not supported with ICU provider",
                        encoding_name(encoding)
                    ),
                ));
            }
            if locale.is_none() {
                return Err(failure("22023", "LOCALE or ICU_LOCALE must be specified"));
            }
            return Err(failure("0A000", "ICU is not supported in this build"));
        }
        _ => None,
    };
    if template != "template0" {
        let hint = |what: &str| {
            format!(
                "Use the same {what} as in the template database, or use template0 as template."
            )
        };
        if encoding != source.encoding {
            return Err(with_hint(
                "22023",
                format!(
                    "new encoding ({}) is incompatible with the encoding of the template database \
                     ({})",
                    encoding_name(encoding),
                    encoding_name(source.encoding)
                ),
                &hint("encoding"),
            ));
        }
        if collate != source.collate {
            return Err(with_hint(
                "22023",
                format!(
                    "new collation ({collate}) is incompatible with the collation of the template \
                     database ({})",
                    source.collate
                ),
                &hint("collation"),
            ));
        }
        if ctype != source.ctype {
            return Err(with_hint(
                "22023",
                format!(
                    "new LC_CTYPE ({ctype}) is incompatible with the LC_CTYPE of the template \
                     database ({})",
                    source.ctype
                ),
                &hint("LC_CTYPE"),
            ));
        }
        if provider != source.provider {
            return Err(with_hint(
                "22023",
                format!(
                    "new locale provider ({}) does not match locale provider of the template \
                     database ({})",
                    provider_name(provider),
                    provider_name(source.provider)
                ),
                &hint("locale provider"),
            ));
        }
    }
    if let Some(version) = &source.collversion
        && found.get("collation_version").is_none()
    {
        let Some(actual) = actual_version(provider) else {
            return Err(failure(
                "XX000",
                format!(
                    "template database \"{template}\" has a collation version, but no actual \
                     collation version could be determined"
                ),
            ));
        };
        if actual != *version {
            return Err(failure(
                "XX000",
                format!("template database \"{template}\" has a collation version mismatch"),
            ));
        }
    }
    let collversion =
        collversion.or_else(|| source.collversion.clone()).or_else(|| actual_version(provider));
    if let Some(space) = text("tablespace")? {
        tablespace(&space, &roles, cx)?;
    }
    if catalog.find(name).is_some() {
        return Err(failure("42P04", format!("database \"{name}\" already exists")));
    }
    let _hold = cx.shared.hold(source.oid);
    busy(
        cx,
        source.oid,
        format!("source database \"{template}\" is being accessed by other users"),
    )?;
    let oid = match oid {
        Some(oid) => {
            if let Some(row) = catalog.by_oid(oid) {
                return Err(failure(
                    "22023",
                    format!("database OID {oid} is already in use by database \"{}\"", row.name),
                ));
            }
            if cx.shared.file_conflict(oid) {
                return Err(failure(
                    "22023",
                    format!("data directory with the specified OID {oid} already exists"),
                ));
            }
            oid
        }
        None => {
            let mut oid = catalog.next_oid();
            while cx.shared.file_conflict(oid) {
                oid += 1;
            }
            oid
        }
    };
    let row = Row {
        oid,
        name: name.to_owned(),
        owner,
        encoding,
        provider,
        template: template_flag,
        allow_connections: allow,
        connlimit,
        collate,
        ctype,
        locale,
        icu_rules,
        collversion,
    };
    supported(&row)?;
    cx.shared.copy_database(source.oid, oid).map_err(written)?;
    let added = cx.shared.databases.change(
        |catalog| {
            catalog.rows.push(row);
            Ok(())
        },
        written,
    );
    if added.is_err() {
        let _ = cx.shared.remove_database(oid);
    }
    added
}

/// The checks of a tablespace in `createdb` and `movedb`. The cluster has the tablespaces of
/// `initdb`, `pg_default` and `pg_global`, which the bootstrap superuser owns.
fn tablespace(name: &str, roles: &roles::Catalog, cx: &Context<'_>) -> Result<(), Failure> {
    if name != "pg_default" && name != "pg_global" {
        return Err(failure("42704", format!("tablespace \"{name}\" does not exist")));
    }
    if !roles.has_privs(cx.current, BOOTSTRAP_SUPERUSER) {
        return Err(failure("42501", format!("permission denied for tablespace {name}")));
    }
    if name == "pg_global" {
        return Err(failure("22023", "pg_global cannot be used as default tablespace"));
    }
    Ok(())
}

/// `dropdb`.
fn drop(
    name: &str,
    missing_ok: bool,
    force: bool,
    cx: &Context<'_>,
    out: &mut OutBuf,
) -> Result<(), Failure> {
    let catalog = cx.shared.databases.snapshot();
    let Some(row) = catalog.find(name) else {
        if missing_ok {
            let message = format!("database \"{name}\" does not exist, skipping");
            notice(out, "NOTICE", "00000", &message, &[]);
            return Ok(());
        }
        return Err(missing(name));
    };
    let roles = cx.shared.roles.snapshot();
    if !roles.has_privs(cx.current, row.owner) {
        return Err(not_owner(name));
    }
    if row.template {
        return Err(failure("42809", "cannot drop a template database"));
    }
    if row.oid == cx.database {
        return Err(failure("55006", "cannot drop the currently open database"));
    }
    let _hold = cx.shared.hold(row.oid);
    if force {
        terminate(row.oid, &roles, cx)?;
    }
    busy(cx, row.oid, format!("database \"{name}\" is being accessed by other users"))?;
    let oid = row.oid;
    cx.shared.databases.change(
        |catalog| {
            catalog.rows.retain(|row| row.oid != oid);
            Ok(())
        },
        written,
    )?;
    if let Err(error) = cx.shared.remove_database(oid) {
        notice(out, "WARNING", "01000", &error, &[]);
    }
    Ok(())
}

/// `TerminateOtherDBBackends`: the checks on each other session of the database, then the end of
/// those sessions.
fn terminate(oid: u32, roles: &roles::Catalog, cx: &Context<'_>) -> Result<(), Failure> {
    let me = cx.current;
    for role in cx.shared.others(oid, cx.pid) {
        let detail = if roles.superuser(role) && !roles.superuser(me) {
            "Only roles with the SUPERUSER attribute may terminate processes of roles with the \
             SUPERUSER attribute."
        } else if !roles.has_privs(me, role) {
            "Only roles with privileges of the role whose process is being terminated or with \
             privileges of the \"pg_signal_backend\" role may terminate this process."
        } else {
            continue;
        };
        return Err(with_detail(
            "42501",
            "permission denied to terminate process",
            detail.to_owned(),
        ));
    }
    cx.shared.terminate(oid, cx.pid);
    Ok(())
}

/// `RenameDatabase`.
fn rename(from: &str, to: &str, cx: &Context<'_>) -> Result<(), Failure> {
    has_newline(to)?;
    let catalog = cx.shared.databases.snapshot();
    let row = catalog.find(from).ok_or_else(|| missing(from))?;
    let roles = cx.shared.roles.snapshot();
    if !roles.has_privs(cx.current, row.owner) {
        return Err(not_owner(from));
    }
    if !createdb(&roles, cx.current) {
        return Err(failure("42501", "permission denied to rename database"));
    }
    if catalog.find(to).is_some() {
        return Err(failure("42P04", format!("database \"{to}\" already exists")));
    }
    if row.oid == cx.database {
        return Err(failure("0A000", "current database cannot be renamed"));
    }
    let _hold = cx.shared.hold(row.oid);
    busy(cx, row.oid, format!("database \"{from}\" is being accessed by other users"))?;
    let oid = row.oid;
    cx.shared.databases.change(
        |catalog| {
            if let Some(row) = catalog.by_oid_mut(oid) {
                to.clone_into(&mut row.name);
            }
            Ok(())
        },
        written,
    )
}

/// `AlterDatabaseOwner`.
fn set_owner(name: &str, spec: &Spec, cx: &Context<'_>) -> Result<(), Failure> {
    let roles = cx.shared.roles.snapshot();
    let role_missing = |name: &str| failure("42704", format!("role \"{name}\" does not exist"));
    let owner = match spec {
        Spec::Name(name) => {
            roles.find(name).map(|role| role.oid).ok_or_else(|| role_missing(name))?
        }
        Spec::Public => return Err(role_missing("public")),
        Spec::CurrentRole | Spec::CurrentUser => cx.current,
        Spec::SessionUser => cx.session,
    };
    let catalog = cx.shared.databases.snapshot();
    let row = catalog.find(name).ok_or_else(|| missing(name))?;
    if row.owner == owner {
        return Ok(());
    }
    if !roles.has_privs(cx.current, row.owner) {
        return Err(not_owner(name));
    }
    if !roles.can_set(cx.current, owner) {
        let target = roles.by_oid(owner).map(|role| role.name.as_str()).unwrap_or_default();
        return Err(failure("42501", format!("must be able to SET ROLE \"{target}\"")));
    }
    if !createdb(&roles, cx.current) {
        return Err(failure("42501", "permission denied to change owner of database"));
    }
    let oid = row.oid;
    cx.shared.databases.change(
        |catalog| {
            catalog.set_owner(oid, owner);
            Ok(())
        },
        written,
    )
}

/// `AlterDatabase`, and `movedb` for the option `tablespace`.
fn alter(name: &str, list: &[Opt], offset: usize, cx: &Context<'_>) -> Result<(), Failure> {
    let mut found = Found(Vec::new());
    for opt in list {
        match opt.name.as_str() {
            key @ ("is_template" | "allow_connections" | "connection_limit" | "tablespace") => {
                if found.get(key).is_some() {
                    return Err(conflicting(opt, offset));
                }
                found.0.push(opt);
            }
            other => {
                return Err(at(
                    failure("42601", format!("option \"{other}\" not recognized")),
                    offset,
                    opt,
                ));
            }
        }
    }
    if let Some(opt) = found.get("tablespace") {
        if list.len() != 1 {
            return Err(at(
                failure("0A000", "option \"tablespace\" cannot be specified with other options"),
                offset,
                opt,
            ));
        }
        prevent(cx, "ALTER DATABASE SET TABLESPACE")?;
        return move_database(name, &string(opt)?, cx);
    }
    let template = found.arg("is_template").map(boolean).transpose()?.unwrap_or(false);
    let allow = found.arg("allow_connections").map(boolean).transpose()?.unwrap_or(true);
    let connlimit = found.arg("connection_limit").map(int32).transpose()?.unwrap_or(-1);
    if connlimit < -1 {
        return Err(failure("22023", format!("invalid connection limit: {connlimit}")));
    }
    let catalog = cx.shared.databases.snapshot();
    let row = catalog.find(name).ok_or_else(|| missing(name))?;
    if !cx.shared.roles.snapshot().has_privs(cx.current, row.owner) {
        return Err(not_owner(name));
    }
    if !allow && row.oid == cx.database {
        return Err(failure("22023", "cannot disallow connections for current database"));
    }
    let oid = row.oid;
    let has = |key: &str| found.get(key).is_some();
    let (set_template, set_allow, set_limit) =
        (has("is_template"), has("allow_connections"), has("connection_limit"));
    cx.shared.databases.change(
        |catalog| {
            if let Some(row) = catalog.by_oid_mut(oid) {
                if set_template {
                    row.template = template;
                }
                if set_allow {
                    row.allow_connections = allow;
                }
                if set_limit {
                    row.connlimit = connlimit;
                }
            }
            Ok(())
        },
        written,
    )
}

/// `movedb`. The cluster has one tablespace for databases, so a database is always in it.
fn move_database(name: &str, space: &str, cx: &Context<'_>) -> Result<(), Failure> {
    let catalog = cx.shared.databases.snapshot();
    let row = catalog.find(name).ok_or_else(|| missing(name))?;
    let roles = cx.shared.roles.snapshot();
    if !roles.has_privs(cx.current, row.owner) {
        return Err(not_owner(name));
    }
    if row.oid == cx.database {
        return Err(failure(
            "55006",
            "cannot change the tablespace of the currently open database",
        ));
    }
    tablespace(space, &roles, cx)
}

/// `AlterDatabaseRefreshColl`. The versions of rudb do not change.
fn refresh(name: &str, cx: &Context<'_>, out: &mut OutBuf) -> Result<(), Failure> {
    let catalog: std::sync::Arc<Catalog> = cx.shared.databases.snapshot();
    let row = catalog.find(name).ok_or_else(|| missing(name))?;
    if !cx.shared.roles.snapshot().has_privs(cx.current, row.owner) {
        return Err(not_owner(name));
    }
    notice(out, "NOTICE", "00000", "version has not changed", &[]);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::{Arg, Opt, Statement, parse};
    use crate::session::role::Spec;

    fn statement(sql: &str) -> Result<Statement, (String, usize)> {
        parse(sql).expect("a statement on the databases").statement.map_err(|e| (e.message, e.at))
    }

    fn opt(name: &str, arg: Option<Arg>, at: usize) -> Opt {
        Opt { name: name.to_owned(), arg, at }
    }

    #[test]
    fn other_statements_go_to_the_engine() {
        assert!(parse("create table t (a int)").is_none());
        assert!(parse("drop role r").is_none());
        assert!(parse("select 1").is_none());
    }

    #[test]
    fn the_forms() {
        let text = |s: &str| Some(Arg::Text(s.to_owned()));
        assert_eq!(
            statement("create database d with owner = u connection limit -5 is_template on x 1.5"),
            Ok(Statement::Create {
                name: "d".to_owned(),
                options: vec![
                    opt("owner", text("u"), 23),
                    opt("connection_limit", Some(Arg::Integer(-5)), 33),
                    opt("is_template", text("on"), 53),
                    opt("x", Some(Arg::Float("1.5".to_owned())), 68),
                ],
            })
        );
        assert_eq!(
            statement("create database d template default encoding 'UTF8';"),
            Ok(Statement::Create {
                name: "d".to_owned(),
                options: vec![opt("template", None, 18), opt("encoding", text("UTF8"), 35)],
            })
        );
        assert_eq!(
            statement("create database d oid -2147483648"),
            Ok(Statement::Create {
                name: "d".to_owned(),
                options: vec![opt("oid", Some(Arg::Float("-2147483648".to_owned())), 18)],
            })
        );
        assert_eq!(
            statement("drop database if exists d with (force, force)"),
            Ok(Statement::Drop { name: "d".to_owned(), missing_ok: true, force: true })
        );
        assert_eq!(
            statement("drop database if"),
            Ok(Statement::Drop { name: "if".to_owned(), missing_ok: false, force: false })
        );
        assert_eq!(
            statement("alter database d rename to e"),
            Ok(Statement::Rename { from: "d".to_owned(), to: "e".to_owned() })
        );
        assert_eq!(
            statement("alter database d owner to current_user"),
            Ok(Statement::Owner { name: "d".to_owned(), owner: Spec::CurrentUser })
        );
        assert_eq!(
            statement("alter database d set tablespace pg_default"),
            Ok(Statement::Alter {
                name: "d".to_owned(),
                options: vec![opt("tablespace", text("pg_default"), 32)],
            })
        );
        assert_eq!(
            statement("alter database d"),
            Ok(Statement::Alter { name: "d".to_owned(), options: vec![] })
        );
        assert_eq!(
            statement("alter database d set work_mem to '1MB'"),
            Ok(Statement::Unsupported("ALTER DATABASE SET is not supported yet"))
        );
        assert_eq!(
            statement("alter database d refresh collation version"),
            Ok(Statement::Refresh { name: "d".to_owned() })
        );
    }

    #[test]
    fn the_errors_of_the_grammar() {
        let error = |sql: &str| statement(sql).expect_err(sql);
        assert_eq!(error("drop database d3, d4"), ("syntax error at or near \",\"".to_owned(), 16));
        assert_eq!(
            error("drop database d3 (bogus)"),
            ("syntax error at or near \"bogus\"".to_owned(), 18)
        );
        assert_eq!(
            error("create database select"),
            ("syntax error at or near \"select\"".to_owned(), 16)
        );
        assert_eq!(
            error("create database d select 1"),
            ("syntax error at or near \"select\"".to_owned(), 18)
        );
        assert_eq!(
            error("create database d owner"),
            ("syntax error at end of input".to_owned(), 23)
        );
    }
}
