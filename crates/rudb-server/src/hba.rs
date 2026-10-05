//! `pg_hba.conf` and `pg_ident.conf`: the files that tell which connections may log in and how,
//! as `hba.c` of PostgreSQL reads them.
//!
//! Both files go through one tokenizer. A token ends at a space, a tab or a comma, double quotes
//! keep a token together, `""` in quotes is one quote, an unquoted `#` starts a comment, and a
//! backslash at the end of a line joins the next line. A field is a list of tokens with commas
//! between them. `@file` reads the tokens of a file into the field, and a line of two fields that
//! starts with `include`, `include_if_exists` or `include_dir` reads the lines of other files.
//!
//! The server loads the files at start. Each error goes to the log with the line of the file as
//! its context, and the rest of the file is still checked, so one start shows every error. A
//! `pg_hba.conf` with an error or with no entry stops the start. A `pg_ident.conf` with an error
//! only goes to the log, and then no map matches.
//!
//! The methods are those of PostgreSQL. `gss`, `sspi`, `pam`, `bsd` and `ldap` are methods of
//! builds with other libraries, and a line with one of them gets the error of such a build.
//! `oauth` needs a validator library, which `rudb-server` cannot load, so a line with it gets the
//! error of a server with no `oauth_validator_libraries`.

use std::cell::RefCell;
use std::ffi::CStr;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, ToSocketAddrs};
use std::path::{Component, Path, PathBuf};

use rudb_regex::Regex;

use crate::roles::Catalog;
use crate::tls::os_text;

/// `CONF_FILE_MAX_DEPTH`: how deep files can include files.
const MAX_DEPTH: usize = 10;

/// One token of a field.
#[derive(Debug, Clone)]
struct Token {
    text: String,
    /// The token had a double quote before its first character. A quoted token is never a
    /// keyword, a group or a file.
    quoted: bool,
    /// The compiled pattern of a token that starts with a slash.
    regex: Option<Regex>,
}

impl Token {
    fn new(text: String, quoted: bool) -> Token {
        Token { text, quoted, regex: None }
    }

    /// `token_is_keyword`.
    fn keyword(&self, word: &str) -> bool {
        !self.quoted && self.text == word
    }

    /// The role of a `+role` token, `token_is_member_check`.
    fn group(&self) -> Option<&str> {
        self.text.strip_prefix('+').filter(|_| !self.quoted)
    }

    /// `regcomp_auth_token`: compiles the pattern of a token that starts with a slash.
    fn compile(&mut self) -> Result<(), String> {
        let Some(pattern) = self.text.strip_prefix('/') else {
            return Ok(());
        };
        match Regex::new(pattern) {
            Ok(regex) => {
                self.regex = Some(regex);
                Ok(())
            }
            Err(e) => Err(format!("invalid regular expression \"{pattern}\": {}", e.message())),
        }
    }
}

/// One line of a file after the tokenizer, `TokenizedAuthLine`.
#[derive(Debug)]
struct TokenLine {
    file: String,
    number: usize,
    raw: String,
    fields: Vec<Vec<Token>>,
    /// The line had an error, which is in the log already.
    failed: bool,
}

/// A message for the log at the level `LOG`, with its `DETAIL`, `HINT` and `CONTEXT` lines.
fn message(text: &str, hint: Option<&str>, context: &[String]) -> String {
    let mut out = text.to_owned();
    if let Some(hint) = hint {
        out.push_str("\nHINT:  ");
        out.push_str(hint);
    }
    for (at, line) in context.iter().enumerate() {
        out.push_str(if at == 0 { "\nCONTEXT:  " } else { "\n" });
        out.push_str(line);
    }
    out
}

/// The context of an error on a line, `line N of configuration file "F"`.
fn line_context(number: usize, file: &str) -> String {
    format!("line {number} of configuration file \"{file}\"")
}

/// The tokenizer of the authentication files, with the log of the errors that it finds.
struct Tokenizer<'a> {
    log: &'a mut Vec<String>,
    /// The lines that the tokenizer is in, the innermost last, for the context of an error.
    stack: Vec<String>,
}

/// A file that did not open: the error for the line, and whether the file is not there.
struct OpenError {
    text: String,
    missing: bool,
}

impl Tokenizer<'_> {
    fn context(&self) -> Vec<String> {
        self.stack.iter().rev().cloned().collect()
    }

    fn error(&mut self, text: &str) {
        let line = message(text, None, &self.context());
        self.log.push(line);
    }

    /// `open_auth_file`.
    fn open(&mut self, path: &str, depth: usize) -> Result<String, OpenError> {
        if depth > MAX_DEPTH {
            let text = format!("could not open file \"{path}\": maximum nesting depth exceeded");
            self.error(&text);
            return Err(OpenError { text, missing: false });
        }
        match std::fs::read(path) {
            Ok(bytes) => Ok(String::from_utf8_lossy(&bytes).into_owned()),
            Err(e) => {
                let text = format!("could not open file \"{path}\": {}", os_text(&e));
                self.error(&text);
                Err(OpenError { text, missing: e.kind() == std::io::ErrorKind::NotFound })
            }
        }
    }

    /// `tokenize_auth_file`: adds the lines of `text`, the file `file`, to `lines`.
    fn file(&mut self, file: &str, text: &str, lines: &mut Vec<TokenLine>, depth: usize) {
        let mut number = 1;
        let mut rest = text;
        while !rest.is_empty() {
            // One line with its continuations. The backslash must come after the place of the
            // last continuation, so `\\` and two line ends do not join three lines.
            let mut buf = String::new();
            let mut continuations = 0;
            let mut last = 0;
            while !rest.is_empty() {
                let end = rest.find('\n').map_or(rest.len(), |at| at + 1);
                buf.push_str(rest[..end].trim_end_matches(['\n', '\r']));
                rest = &rest[end..];
                if buf.len() > last && buf.ends_with('\\') {
                    buf.pop();
                    last = buf.len();
                    continuations += 1;
                    continue;
                }
                break;
            }
            self.stack.push(line_context(number, file));
            self.line(file, number, buf, lines, depth);
            self.stack.pop();
            number += continuations + 1;
        }
    }

    /// The fields of one line, and the include directives.
    fn line(
        &mut self,
        file: &str,
        number: usize,
        raw: String,
        lines: &mut Vec<TokenLine>,
        depth: usize,
    ) {
        let mut fields = Vec::new();
        let mut error = None;
        let mut at = 0;
        let bytes = raw.as_bytes();
        while at < bytes.len() && error.is_none() {
            let field = self.field(file, bytes, &mut at, depth, &mut error);
            if !field.is_empty() {
                fields.push(field);
            }
        }
        if fields.is_empty() && error.is_none() {
            return;
        }
        if error.is_none() && fields.len() == 2 {
            let first = fields[0][0].text.clone();
            let second = fields[1][0].text.clone();
            match first.as_str() {
                "include" | "include_if_exists" => {
                    let missing_ok = first == "include_if_exists";
                    match self.include(file, &second, lines, depth + 1, missing_ok) {
                        Ok(()) => return,
                        Err(text) => error = Some(text),
                    }
                }
                "include_dir" => match self.include_dir(file, &second, lines, depth) {
                    Ok(()) => return,
                    Err(text) => error = Some(text),
                },
                _ => {}
            }
        }
        lines.push(TokenLine {
            file: file.to_owned(),
            number,
            raw,
            fields,
            failed: error.is_some(),
        });
    }

    /// `next_field_expand`: the tokens of one field, with `@file` read in.
    fn field(
        &mut self,
        file: &str,
        line: &[u8],
        at: &mut usize,
        depth: usize,
        error: &mut Option<String>,
    ) -> Vec<Token> {
        let mut tokens = Vec::new();
        while let Some((text, quoted, comma)) = next_token(line, at) {
            if !quoted && text.len() > 1 && text.starts_with('@') {
                self.expand(file, &text[1..], &mut tokens, depth + 1, error);
            } else {
                tokens.push(Token::new(text, quoted));
            }
            if !comma || error.is_some() {
                break;
            }
        }
        tokens
    }

    /// `tokenize_expand_file`: the tokens of every line of the file `name` into `tokens`.
    fn expand(
        &mut self,
        outer: &str,
        name: &str,
        tokens: &mut Vec<Token>,
        depth: usize,
        error: &mut Option<String>,
    ) {
        let path = absolute_location(name, Some(outer));
        let text = match self.open(&path, depth) {
            Ok(text) => text,
            Err(failed) => {
                *error = Some(failed.text);
                return;
            }
        };
        let mut inner = Vec::new();
        self.file(&path, &text, &mut inner, depth);
        for line in inner {
            if line.failed {
                *error = Some(format!("error in file \"{}\"", line.file));
                break;
            }
            tokens.extend(line.fields.into_iter().flatten());
        }
    }

    /// `tokenize_include_file`.
    fn include(
        &mut self,
        outer: &str,
        name: &str,
        lines: &mut Vec<TokenLine>,
        depth: usize,
        missing_ok: bool,
    ) -> Result<(), String> {
        let path = absolute_location(name, Some(outer));
        match self.open(&path, depth) {
            Ok(text) => {
                self.file(&path, &text, lines, depth);
                Ok(())
            }
            Err(failed) if failed.missing && missing_ok => {
                self.error(&format!("skipping missing authentication file \"{path}\""));
                Ok(())
            }
            Err(failed) => Err(failed.text),
        }
    }

    /// The `include_dir` directive: each file of the directory that ends in `.conf`, in the
    /// order of the names.
    fn include_dir(
        &mut self,
        outer: &str,
        name: &str,
        lines: &mut Vec<TokenLine>,
        depth: usize,
    ) -> Result<(), String> {
        let files = self.conf_files(outer, name)?;
        let mut errors = Vec::new();
        for path in files {
            if let Err(text) = self.include(outer, &path, lines, depth + 1, false) {
                errors.push(text);
            }
        }
        if errors.is_empty() { Ok(()) } else { Err(errors.join("\n")) }
    }

    /// `GetConfFilesInDir`.
    fn conf_files(&mut self, outer: &str, name: &str) -> Result<Vec<String>, String> {
        if name.trim_matches([' ', '\t', '\r', '\n']).is_empty() {
            self.error(&format!("empty configuration directory name: \"{name}\""));
            return Err("empty configuration directory name".to_owned());
        }
        let dir = absolute_location(name, Some(outer));
        let entries = match std::fs::read_dir(&dir) {
            Ok(entries) => entries,
            Err(e) => {
                self.error(&format!(
                    "could not open configuration directory \"{dir}\": {}",
                    os_text(&e)
                ));
                return Err(format!("could not open directory \"{dir}\""));
            }
        };
        let mut files = Vec::new();
        for entry in entries.flatten() {
            let file = entry.file_name();
            let file = file.to_string_lossy();
            if file.len() < 6 || file.starts_with('.') || !file.ends_with(".conf") {
                continue;
            }
            let path = canonical(&Path::new(&dir).join(&*file));
            match std::fs::metadata(&path) {
                Ok(meta) if meta.is_dir() => {}
                Ok(_) => files.push(path),
                Err(e) => {
                    self.error(&format!("could not stat file \"{path}\": {}", os_text(&e)));
                    return Err(format!("could not stat file \"{path}\""));
                }
            }
        }
        files.sort();
        Ok(files)
    }
}

/// `next_token`: the next token of `line` from `at`, whether it started with a quote, and
/// whether a comma ends it. `None` at the end of the line or at a comment.
fn next_token(line: &[u8], at: &mut usize) -> Option<(String, bool, bool)> {
    let blank = |c: u8| c == b' ' || c == b'\t';
    while *at < line.len() && (blank(line[*at]) || line[*at] == b',') {
        *at += 1;
    }
    let mut buf = Vec::new();
    let (mut in_quote, mut was_quote, mut saw_quote) = (false, false, false);
    let mut initial_quote = false;
    let mut comma = false;
    while *at < line.len() && (!blank(line[*at]) || in_quote) {
        let c = line[*at];
        if c == b'#' && !in_quote {
            *at = line.len();
            break;
        }
        if c == b',' && !in_quote {
            comma = true;
            break;
        }
        if c != b'"' || was_quote {
            buf.push(c);
        }
        was_quote = in_quote && c == b'"' && !was_quote;
        if c == b'"' {
            in_quote = !in_quote;
            saw_quote = true;
            if buf.is_empty() {
                initial_quote = true;
            }
        }
        *at += 1;
    }
    (saw_quote || !buf.is_empty())
        .then(|| (String::from_utf8_lossy(&buf).into_owned(), initial_quote, comma))
}

/// `AbsoluteConfigLocation`: a path relative to the directory of the file that names it.
fn absolute_location(location: &str, calling: Option<&str>) -> String {
    let path = Path::new(location);
    if path.is_absolute() {
        return location.to_owned();
    }
    let base = calling.and_then(|file| Path::new(file).parent()).unwrap_or(Path::new("/"));
    canonical(&base.join(path))
}

/// `canonicalize_path`, which works on the text and does not follow links.
pub(crate) fn canonical(path: &Path) -> String {
    let mut parts: Vec<&str> = Vec::new();
    let absolute = path.is_absolute();
    let mut ups = 0;
    for component in path.components() {
        match component {
            Component::Normal(part) => parts.push(part.to_str().unwrap_or_default()),
            Component::ParentDir => {
                if parts.pop().is_none() && !absolute {
                    ups += 1;
                }
            }
            Component::CurDir | Component::RootDir | Component::Prefix(_) => {}
        }
    }
    let mut out = if absolute { "/".to_owned() } else { "../".repeat(ups) };
    out.push_str(&parts.join("/"));
    if out.is_empty() {
        out.push('.');
    }
    if out.len() > 1 && out.ends_with('/') {
        out.pop();
    }
    out
}

/// Reads one authentication file from its top, `open_auth_file` and `tokenize_auth_file`.
fn tokenize(path: &str, log: &mut Vec<String>) -> Option<Vec<TokenLine>> {
    let mut tokenizer = Tokenizer { log, stack: Vec::new() };
    let text = tokenizer.open(path, 0).ok()?;
    let mut lines = Vec::new();
    tokenizer.file(path, &text, &mut lines, 0);
    Some(lines)
}

/// The kind of connection of a line.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Kind {
    Local,
    Host,
    HostSsl,
    HostNoSsl,
    HostGssEnc,
    HostNoGssEnc,
}

/// The address of a `host` line.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Address {
    /// A `local` line has none.
    None,
    All,
    SameHost,
    SameNet,
    Mask(IpAddr, IpAddr),
    /// A host name, or a suffix of host names when it starts with a dot.
    Name(String),
}

/// The authentication method of a line.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Method {
    Trust,
    Reject,
    Ident,
    Peer,
    Password,
    Md5,
    Scram,
    Cert,
    OAuth,
}

impl Method {
    /// `auth_failed`: the text of the error after a failed authentication, and its SQLSTATE.
    pub(crate) fn failed(self, user: &str) -> (&'static str, String) {
        match self {
            Method::Reject => {
                ("28000", format!("authentication failed for user \"{user}\": host rejected"))
            }
            Method::Trust => {
                ("28000", format!("\"trust\" authentication failed for user \"{user}\""))
            }
            Method::Ident => ("28000", format!("Ident authentication failed for user \"{user}\"")),
            Method::Peer => ("28000", format!("Peer authentication failed for user \"{user}\"")),
            Method::Password | Method::Md5 | Method::Scram => {
                ("28P01", format!("password authentication failed for user \"{user}\""))
            }
            Method::Cert => {
                ("28000", format!("certificate authentication failed for user \"{user}\""))
            }
            Method::OAuth => {
                ("28000", format!("OAuth bearer authentication failed for user \"{user}\""))
            }
        }
    }
}

/// The `clientcert` option.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ClientCert {
    Off,
    VerifyCa,
    VerifyFull,
}

/// The `clientname` option.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ClientName {
    Cn,
    Dn,
}

/// One line of `pg_hba.conf`, `HbaLine`.
#[derive(Debug, Clone)]
pub(crate) struct HbaLine {
    pub(crate) file: String,
    pub(crate) number: usize,
    pub(crate) raw: String,
    pub(crate) kind: Kind,
    databases: Vec<Token>,
    roles: Vec<Token>,
    address: Address,
    pub(crate) method: Method,
    pub(crate) map: Option<String>,
    pub(crate) clientcert: ClientCert,
    pub(crate) clientname: ClientName,
}

/// The methods that only other builds of PostgreSQL have.
const OTHER_BUILDS: [&str; 5] = ["gss", "sspi", "pam", "bsd", "ldap"];

/// The options of methods that only other builds have, with the text of the valid methods.
const OTHER_OPTIONS: [(&str, &str); 19] = [
    ("pamservice", "pam"),
    ("pam_use_hostname", "pam"),
    ("ldapurl", "ldap"),
    ("ldaptls", "ldap"),
    ("ldapscheme", "ldap"),
    ("ldapserver", "ldap"),
    ("ldapport", "ldap"),
    ("ldapbinddn", "ldap"),
    ("ldapbindpasswd", "ldap"),
    ("ldapsearchattribute", "ldap"),
    ("ldapsearchfilter", "ldap"),
    ("ldapbasedn", "ldap"),
    ("ldapprefix", "ldap"),
    ("ldapsuffix", "ldap"),
    ("krb_realm", "gssapi and sspi"),
    ("include_realm", "gssapi and sspi"),
    ("compat_realm", "sspi"),
    ("upn_username", "sspi"),
    ("radiusservers", "radius"),
];

/// The settings of the server that the parse of a line needs.
#[derive(Debug, Clone, Copy)]
pub(crate) struct ParseSettings {
    /// `ssl` is on.
    pub(crate) ssl: bool,
}

/// `parse_hba_line`. An error goes to `log` and gives `None`.
fn parse_hba_line(
    line: &TokenLine,
    settings: ParseSettings,
    log: &mut Vec<String>,
) -> Option<HbaLine> {
    let context = [line_context(line.number, &line.file)];
    let mut fail = |text: &str, hint: Option<&str>| {
        log.push(message(text, hint, &context));
        None
    };
    let mut fields = line.fields.iter();
    let kind_field = fields.next()?;
    if kind_field.len() > 1 {
        return fail(
            "multiple values specified for connection type",
            Some("Specify exactly one connection type per line."),
        );
    }
    let kind = match kind_field[0].text.as_str() {
        "local" => Kind::Local,
        "host" => Kind::Host,
        "hostssl" => {
            if !settings.ssl {
                // The line still loads. It can never match.
                log.push(message(
                    "hostssl record cannot match because SSL is disabled",
                    Some("Set \"ssl = on\" in postgresql.conf."),
                    &context,
                ));
            }
            Kind::HostSsl
        }
        "hostnossl" => Kind::HostNoSsl,
        "hostgssenc" => {
            log.push(message(
                "hostgssenc record cannot match because GSSAPI is not supported by this build",
                None,
                &context,
            ));
            Kind::HostGssEnc
        }
        "hostnogssenc" => Kind::HostNoGssEnc,
        other => return fail(&format!("invalid connection type \"{other}\""), None),
    };
    let mut fail = |text: &str, hint: Option<&str>| {
        log.push(message(text, hint, &context));
        None
    };
    let Some(databases) = fields.next() else {
        return fail("end-of-line before database specification", None);
    };
    let mut databases = databases.clone();
    for token in &mut databases {
        if let Err(text) = token.compile() {
            return fail(&text, None);
        }
    }
    let Some(roles) = fields.next() else {
        return fail("end-of-line before role specification", None);
    };
    let mut roles = roles.clone();
    for token in &mut roles {
        if let Err(text) = token.compile() {
            return fail(&text, None);
        }
    }
    let address = if kind == Kind::Local {
        Address::None
    } else {
        let Some(tokens) = fields.next() else {
            return fail("end-of-line before IP address specification", None);
        };
        if tokens.len() > 1 {
            return fail(
                "multiple values specified for host address",
                Some("Specify one address range per line."),
            );
        }
        let token = &tokens[0];
        if token.keyword("all") {
            Address::All
        } else if token.keyword("samehost") {
            Address::SameHost
        } else if token.keyword("samenet") {
            Address::SameNet
        } else {
            let (host, bits) = match token.text.split_once('/') {
                Some((host, bits)) => (host, Some(bits)),
                None => (token.text.as_str(), None),
            };
            match (numeric_host(host), bits) {
                (None, Some(_)) => {
                    return fail(
                        &format!(
                            "specifying both host name and CIDR mask is invalid: \"{}\"",
                            token.text
                        ),
                        None,
                    );
                }
                (None, None) => Address::Name(host.to_owned()),
                (Some(ip), Some(bits)) => match cidr_mask(bits, ip.is_ipv4()) {
                    Some(mask) => Address::Mask(ip, mask),
                    None => {
                        return fail(
                            &format!("invalid CIDR mask in address \"{}\"", token.text),
                            None,
                        );
                    }
                },
                (Some(ip), None) => {
                    let Some(tokens) = fields.next() else {
                        return fail(
                            "end-of-line before netmask specification",
                            Some(
                                "Specify an address range in CIDR notation, or provide a \
                                 separate netmask.",
                            ),
                        );
                    };
                    if tokens.len() > 1 {
                        return fail("multiple values specified for netmask", None);
                    }
                    let Some(mask) = numeric_host(&tokens[0].text) else {
                        return fail(
                            &format!(
                                "invalid IP mask \"{}\": {}",
                                tokens[0].text,
                                gai_error(libc::EAI_NONAME)
                            ),
                            None,
                        );
                    };
                    if mask.is_ipv4() != ip.is_ipv4() {
                        return fail("IP address and mask do not match", None);
                    }
                    Address::Mask(ip, mask)
                }
            }
        }
    };
    let Some(tokens) = fields.next() else {
        return fail("end-of-line before authentication method", None);
    };
    if tokens.len() > 1 {
        return fail(
            "multiple values specified for authentication type",
            Some("Specify exactly one authentication type per line."),
        );
    }
    let name = tokens[0].text.as_str();
    let mut method = match name {
        "trust" => Method::Trust,
        "ident" => Method::Ident,
        "peer" => Method::Peer,
        "password" => Method::Password,
        "reject" => Method::Reject,
        "md5" => Method::Md5,
        "scram-sha-256" => Method::Scram,
        "cert" => Method::Cert,
        "oauth" => Method::OAuth,
        _ if OTHER_BUILDS.contains(&name) => {
            return fail(
                &format!("invalid authentication method \"{name}\": not supported by this build"),
                None,
            );
        }
        _ => return fail(&format!("invalid authentication method \"{name}\""), None),
    };
    if kind == Kind::Local && method == Method::Ident {
        method = Method::Peer;
    }
    if kind != Kind::Local && method == Method::Peer {
        return fail("peer authentication is only supported on local sockets", None);
    }
    if kind != Kind::HostSsl && method == Method::Cert {
        return fail("cert authentication is only supported on hostssl connections", None);
    }
    let mut parsed = HbaLine {
        file: line.file.clone(),
        number: line.number,
        raw: line.raw.clone(),
        kind,
        databases,
        roles,
        address,
        method,
        map: None,
        clientcert: ClientCert::Off,
        clientname: ClientName::Cn,
    };
    let mut oauth = OAuthOptions::default();
    for token in fields.flatten() {
        let Some((name, value)) = token.text.split_once('=') else {
            return fail(
                &format!("authentication option not in name=value format: {}", token.text),
                None,
            );
        };
        if let Err(text) = parse_option(name, value, &mut parsed, &mut oauth) {
            return fail(&text, None);
        }
    }
    if method == Method::Cert {
        parsed.clientcert = ClientCert::VerifyFull;
    }
    if method == Method::OAuth {
        for (set, option) in [(oauth.scope, "scope"), (oauth.issuer, "issuer")] {
            if !set {
                return fail(
                    &format!(
                        "authentication method \"oauth\" requires argument \"{option}\" to be set"
                    ),
                    None,
                );
            }
        }
        return fail(
            "parameter \"oauth_validator_libraries\" must be set for authentication method \
             \"oauth\"",
            None,
        );
    }
    Some(parsed)
}

/// The `oauth` options that the checks after the options look at.
#[derive(Debug, Default)]
struct OAuthOptions {
    scope: bool,
    issuer: bool,
}

/// `parse_hba_auth_opt`.
fn parse_option(
    name: &str,
    value: &str,
    line: &mut HbaLine,
    oauth: &mut OAuthOptions,
) -> Result<(), String> {
    let only = |methods: &str| {
        Err(format!(
            "authentication option \"{name}\" is only valid for authentication methods {methods}"
        ))
    };
    match name {
        "map" => {
            if !matches!(line.method, Method::Ident | Method::Peer | Method::Cert | Method::OAuth) {
                return only("ident, peer, gssapi, sspi, cert, and oauth");
            }
            line.map = Some(value.to_owned());
        }
        "clientcert" => {
            if line.kind != Kind::HostSsl {
                return Err("clientcert can only be configured for \"hostssl\" rows".to_owned());
            }
            line.clientcert = match value {
                "verify-full" => ClientCert::VerifyFull,
                "verify-ca" if line.method == Method::Cert => {
                    return Err("clientcert only accepts \"verify-full\" when using \"cert\" \
                                authentication"
                        .to_owned());
                }
                "verify-ca" => ClientCert::VerifyCa,
                _ => return Err(format!("invalid value for clientcert: \"{value}\"")),
            };
        }
        "clientname" => {
            if line.kind != Kind::HostSsl {
                return Err("clientname can only be configured for \"hostssl\" rows".to_owned());
            }
            line.clientname = match value {
                "CN" => ClientName::Cn,
                "DN" => ClientName::Dn,
                _ => return Err(format!("invalid value for clientname: \"{value}\"")),
            };
        }
        "issuer" | "scope" | "validator" | "delegate_ident_mapping" => {
            if line.method != Method::OAuth {
                return only("oauth");
            }
            oauth.issuer |= name == "issuer";
            oauth.scope |= name == "scope";
        }
        _ if name.starts_with("validator.") => {
            if line.method != Method::OAuth {
                return only("oauth");
            }
            let key = &name["validator.".len()..];
            if key.is_empty()
                || !key.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-')
            {
                return Err(format!("invalid OAuth validator option name: \"{name}\""));
            }
        }
        _ => match OTHER_OPTIONS.iter().find(|(option, _)| *option == name) {
            // RADIUS left PostgreSQL in version 18, so its options are not known any more.
            Some((_, methods)) if *methods != "radius" => return only(methods),
            _ => return Err(format!("unrecognized authentication option name: \"{name}\"")),
        },
    }
    Ok(())
}

/// The address of a numeric host, as `getaddrinfo` with `AI_NUMERICHOST` reads it: IPv6, or
/// IPv4 in the forms of `inet_aton`, which also has `127.1` and hexadecimal parts.
fn numeric_host(text: &str) -> Option<IpAddr> {
    if text.contains(':') {
        return text.parse::<Ipv6Addr>().ok().map(IpAddr::V6);
    }
    let parts: Vec<&str> = text.split('.').collect();
    if parts.is_empty() || parts.len() > 4 {
        return None;
    }
    let mut values = Vec::with_capacity(parts.len());
    for part in &parts {
        let (digits, radix) =
            if let Some(hex) = part.strip_prefix("0x").or_else(|| part.strip_prefix("0X")) {
                (hex, 16)
            } else if part.len() > 1 && part.starts_with('0') {
                (&part[1..], 8)
            } else {
                (*part, 10)
            };
        if digits.is_empty() && radix != 16 {
            return None;
        }
        let value = if digits.is_empty() { 0 } else { u32::from_str_radix(digits, radix).ok()? };
        values.push(value);
    }
    let (last, head) = values.split_last()?;
    if head.iter().any(|&v| v > 255) {
        return None;
    }
    let room = 32 - 8 * head.len() as u32;
    if room < 32 && *last >= 1 << room {
        return None;
    }
    let mut address = 0u32;
    for (at, value) in head.iter().enumerate() {
        address |= value << (24 - 8 * at);
    }
    address |= last;
    Some(IpAddr::V4(Ipv4Addr::from(address)))
}

/// `pg_sockaddr_cidr_mask`: the mask of `bits` leading ones, or `None` for bits that are not a
/// number in the range of the family.
fn cidr_mask(bits: &str, v4: bool) -> Option<IpAddr> {
    if bits.is_empty() {
        return None;
    }
    let (negative, digits) = match bits.strip_prefix('-') {
        Some(rest) => (true, rest),
        None => (false, bits.strip_prefix('+').unwrap_or(bits)),
    };
    let digits = digits.trim_start_matches([' ', '\t', '\n', '\r', '\x0b', '\x0c']);
    if digits.is_empty() || !digits.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    let bits: u32 = digits.parse().ok()?;
    if negative && bits != 0 {
        return None;
    }
    if v4 {
        (bits <= 32).then(|| {
            IpAddr::V4(Ipv4Addr::from(if bits == 0 { 0 } else { u32::MAX << (32 - bits) }))
        })
    } else {
        (bits <= 128).then(|| {
            IpAddr::V6(Ipv6Addr::from(if bits == 0 { 0 } else { u128::MAX << (128 - bits) }))
        })
    }
}

/// `gai_strerror`.
fn gai_error(code: i32) -> String {
    // SAFETY: `gai_strerror` returns a pointer to a static string for any code.
    unsafe { CStr::from_ptr(libc::gai_strerror(code)) }.to_string_lossy().into_owned()
}

/// `check_ip`: the address is in the range of the address and the mask.
fn in_range(client: IpAddr, address: IpAddr, mask: IpAddr) -> bool {
    match (client, address, mask) {
        (IpAddr::V4(c), IpAddr::V4(a), IpAddr::V4(m)) => {
            (u32::from(c) ^ u32::from(a)) & u32::from(m) == 0
        }
        (IpAddr::V6(c), IpAddr::V6(a), IpAddr::V6(m)) => {
            (u128::from(c) ^ u128::from(a)) & u128::from(m) == 0
        }
        _ => false,
    }
}

/// The addresses of the interfaces of this machine with their masks, from `getifaddrs`.
fn interfaces() -> Result<Vec<(IpAddr, IpAddr)>, String> {
    let mut list: *mut libc::ifaddrs = std::ptr::null_mut();
    // SAFETY: `list` is a valid place for the pointer to the list.
    if unsafe { libc::getifaddrs(&raw mut list) } != 0 {
        return Err(os_text(&std::io::Error::last_os_error()));
    }
    let mut out = Vec::new();
    let mut at = list;
    while !at.is_null() {
        // SAFETY: `at` is an entry of the list that `getifaddrs` gave, which lives until
        // `freeifaddrs` below.
        let entry = unsafe { &*at };
        // SAFETY: the address and the mask are null or valid socket addresses of the entry.
        let address = unsafe { sockaddr_ip(entry.ifa_addr) };
        if let Some(address) = address {
            // SAFETY: as above.
            let mask = unsafe { sockaddr_ip(entry.ifa_netmask) };
            let full = if address.is_ipv4() {
                IpAddr::V4(Ipv4Addr::from(u32::MAX))
            } else {
                IpAddr::V6(Ipv6Addr::from(u128::MAX))
            };
            let mask = mask.filter(|m| m.is_ipv4() == address.is_ipv4()).unwrap_or(full);
            out.push((address, mask));
        }
        at = entry.ifa_next;
    }
    // SAFETY: `list` came from `getifaddrs` and is freed once.
    unsafe { libc::freeifaddrs(list) };
    Ok(out)
}

/// The IP address of a socket address of the family `AF_INET` or `AF_INET6`.
///
/// # Safety
///
/// `address` is null or points to a valid socket address.
unsafe fn sockaddr_ip(address: *const libc::sockaddr) -> Option<IpAddr> {
    if address.is_null() {
        return None;
    }
    // SAFETY: the caller gives a valid socket address, and the family tells its type.
    unsafe {
        match i32::from((*address).sa_family) {
            libc::AF_INET => {
                let v4 = &*address.cast::<libc::sockaddr_in>();
                Some(IpAddr::V4(Ipv4Addr::from(u32::from_be(v4.sin_addr.s_addr))))
            }
            libc::AF_INET6 => {
                let v6 = &*address.cast::<libc::sockaddr_in6>();
                Some(IpAddr::V6(Ipv6Addr::from(v6.sin6_addr.s6_addr)))
            }
            _ => None,
        }
    }
}

/// The host name of an address from `getnameinfo` with `NI_NAMEREQD`, or the text of the error.
fn reverse_lookup(ip: IpAddr) -> Result<String, String> {
    let mut host = [0 as libc::c_char; 1025];
    // SAFETY: the socket address is built in full here, and `host` is valid for its length.
    let code = unsafe {
        let mut storage = std::mem::zeroed::<libc::sockaddr_storage>();
        let len = match ip {
            IpAddr::V4(v4) => {
                let sin = &mut *(&raw mut storage).cast::<libc::sockaddr_in>();
                sin.sin_family = libc::AF_INET as libc::sa_family_t;
                sin.sin_addr.s_addr = u32::from(v4).to_be();
                #[cfg(any(target_os = "macos", target_os = "freebsd"))]
                {
                    sin.sin_len = size_of::<libc::sockaddr_in>() as u8;
                }
                size_of::<libc::sockaddr_in>()
            }
            IpAddr::V6(v6) => {
                let sin6 = &mut *(&raw mut storage).cast::<libc::sockaddr_in6>();
                sin6.sin6_family = libc::AF_INET6 as libc::sa_family_t;
                sin6.sin6_addr.s6_addr = v6.octets();
                #[cfg(any(target_os = "macos", target_os = "freebsd"))]
                {
                    sin6.sin6_len = size_of::<libc::sockaddr_in6>() as u8;
                }
                size_of::<libc::sockaddr_in6>()
            }
        };
        libc::getnameinfo(
            (&raw const storage).cast(),
            len as libc::socklen_t,
            host.as_mut_ptr(),
            host.len() as libc::socklen_t,
            std::ptr::null_mut(),
            0,
            libc::NI_NAMEREQD,
        )
    };
    if code != 0 {
        return Err(gai_error(code));
    }
    // SAFETY: `getnameinfo` wrote a string with its zero byte into `host`.
    Ok(unsafe { CStr::from_ptr(host.as_ptr()) }.to_string_lossy().into_owned())
}

/// What the server knows about the host name of a client, `remote_hostname_resolv`.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Resolved {
    /// No line asked yet.
    Unknown,
    /// The name of the address, and whether the name gives the address back: `None` before the
    /// check.
    Name(String, Option<bool>),
    /// The reverse lookup failed with this text.
    NoName(String),
    /// The forward lookup of the name failed with this text.
    NoAddress(String, String),
}

/// A connection that wants to log in.
#[derive(Debug)]
pub(crate) struct Client<'a> {
    /// The address of the client, `None` on a Unix socket.
    pub(crate) address: Option<IpAddr>,
    /// The connection uses TLS.
    pub(crate) ssl: bool,
    pub(crate) user: &'a str,
    pub(crate) database: &'a str,
    resolved: RefCell<Resolved>,
}

impl<'a> Client<'a> {
    pub(crate) fn new(
        address: Option<IpAddr>,
        ssl: bool,
        user: &'a str,
        database: &'a str,
    ) -> Client<'a> {
        Client { address, ssl, user, database, resolved: RefCell::new(Resolved::Unknown) }
    }

    /// The text for the host in an error: the address, or `[local]` on a Unix socket.
    pub(crate) fn host(&self) -> String {
        self.address.map_or_else(|| "[local]".to_owned(), |ip| ip.to_string())
    }

    /// The `DETAIL` for the log of a connection that no line matched, from the host name
    /// lookups, `HOSTNAME_LOOKUP_DETAIL`.
    pub(crate) fn lookup_detail(&self) -> Option<String> {
        match &*self.resolved.borrow() {
            Resolved::Unknown => None,
            Resolved::Name(name, Some(true)) => {
                Some(format!("Client IP address resolved to \"{name}\", forward lookup matches."))
            }
            Resolved::Name(name, None) => Some(format!(
                "Client IP address resolved to \"{name}\", forward lookup not checked."
            )),
            Resolved::Name(name, Some(false)) => Some(format!(
                "Client IP address resolved to \"{name}\", forward lookup does not match."
            )),
            Resolved::NoName(error) => {
                Some(format!("Could not resolve client IP address to a host name: {error}."))
            }
            Resolved::NoAddress(name, error) => Some(format!(
                "Could not translate client host name \"{name}\" to IP address: {error}."
            )),
        }
    }

    /// `check_hostname`.
    fn host_matches(&self, ip: IpAddr, pattern: &str) -> bool {
        let mut resolved = self.resolved.borrow_mut();
        if *resolved == Resolved::Unknown {
            *resolved = match reverse_lookup(ip) {
                Ok(name) => Resolved::Name(name, None),
                Err(error) => Resolved::NoName(error),
            };
        }
        let Resolved::Name(name, checked) = &*resolved else {
            return false;
        };
        if *checked == Some(false) {
            return false;
        }
        let matches = match pattern.strip_prefix('.') {
            Some(_) => {
                name.len() >= pattern.len()
                    && name[name.len() - pattern.len()..].eq_ignore_ascii_case(pattern)
            }
            None => name.eq_ignore_ascii_case(pattern),
        };
        if !matches {
            return false;
        }
        if *checked == Some(true) {
            return true;
        }
        let name = name.clone();
        match (name.as_str(), 0).to_socket_addrs() {
            Ok(addresses) => {
                let found = addresses.into_iter().any(|address| address.ip() == ip);
                *resolved = Resolved::Name(name, Some(found));
                found
            }
            Err(e) => {
                *resolved = Resolved::NoAddress(name, lookup_text(&e));
                false
            }
        }
    }
}

/// The text of a failed lookup of a host name, which the standard library gives with a prefix.
fn lookup_text(error: &std::io::Error) -> String {
    let text = error.to_string();
    text.strip_prefix("failed to lookup address information: ").unwrap_or(&text).to_owned()
}

/// The loaded `pg_hba.conf`.
#[derive(Debug, Clone)]
pub(crate) struct Hba {
    pub(crate) lines: Vec<HbaLine>,
}

impl Hba {
    /// `load_hba`. The messages for the log go to `log`. `None` means that the file has an error
    /// or no entry, and the caller adds the `FATAL` or keeps the old file.
    pub(crate) fn load(path: &str, settings: ParseSettings, log: &mut Vec<String>) -> Option<Hba> {
        let lines = tokenize(path, log)?;
        let mut ok = true;
        let mut parsed = Vec::new();
        for line in &lines {
            if line.failed {
                ok = false;
                continue;
            }
            match parse_hba_line(line, settings, log) {
                Some(line) => parsed.push(line),
                None => ok = false,
            }
        }
        if ok && parsed.is_empty() {
            log.push(format!("configuration file \"{path}\" contains no entries"));
            ok = false;
        }
        ok.then_some(Hba { lines: parsed })
    }

    /// `check_hba`: the first line that matches the client, or `None` for the implicit reject.
    pub(crate) fn find(&self, client: &Client<'_>, catalog: &Catalog) -> Option<&HbaLine> {
        let oid = catalog.find(client.user).map(|role| role.oid);
        let mut interfaces_cache: Option<Vec<(IpAddr, IpAddr)>> = None;
        self.lines.iter().find(|line| {
            match (line.kind, client.address) {
                (Kind::Local, None) => {}
                (Kind::Local, Some(_)) | (_, None) => return false,
                (kind, Some(ip)) => {
                    let skip = match kind {
                        Kind::HostNoSsl => client.ssl,
                        Kind::HostSsl => !client.ssl,
                        Kind::HostGssEnc => true,
                        _ => false,
                    };
                    if skip {
                        return false;
                    }
                    let matches = match &line.address {
                        Address::All | Address::None => true,
                        Address::Mask(address, mask) => in_range(ip, *address, *mask),
                        Address::Name(name) => client.host_matches(ip, name),
                        Address::SameHost | Address::SameNet => {
                            if interfaces_cache.is_none() {
                                interfaces_cache = Some(match interfaces() {
                                    Ok(list) => list,
                                    Err(error) => {
                                        crate::server::log(
                                            "LOG",
                                            &format!(
                                                "error enumerating network interfaces: {error}"
                                            ),
                                        );
                                        Vec::new()
                                    }
                                });
                            }
                            let list = interfaces_cache.as_deref().unwrap_or_default();
                            let same_host = line.address == Address::SameHost;
                            list.iter().any(|(address, mask)| {
                                if same_host {
                                    *address == ip
                                } else {
                                    in_range(ip, *address, *mask)
                                }
                            })
                        }
                    };
                    if !matches {
                        return false;
                    }
                }
            }
            check_db(client.database, client.user, oid, &line.databases, catalog)
                && check_role(client.user, oid, &line.roles, catalog)
        })
    }
}

/// `is_member`: the role `oid` is a member of the role named `role`, directly or through other
/// roles. A superuser is not a member of every role here.
fn is_member(catalog: &Catalog, oid: Option<u32>, role: &str) -> bool {
    match (oid, catalog.find(role)) {
        (Some(oid), Some(role)) => catalog.member_of(oid, role.oid),
        _ => false,
    }
}

/// `check_role`.
fn check_role(user: &str, oid: Option<u32>, tokens: &[Token], catalog: &Catalog) -> bool {
    tokens.iter().any(|token| {
        if let Some(group) = token.group() {
            is_member(catalog, oid, group)
        } else if token.keyword("all") {
            true
        } else if let Some(regex) = &token.regex {
            regex.is_match(user)
        } else {
            token.text == user
        }
    })
}

/// `check_db`.
fn check_db(
    database: &str,
    user: &str,
    oid: Option<u32>,
    tokens: &[Token],
    catalog: &Catalog,
) -> bool {
    tokens.iter().any(|token| {
        if token.keyword("all") {
            true
        } else if token.keyword("sameuser") {
            database == user
        } else if token.keyword("samegroup") || token.keyword("samerole") {
            is_member(catalog, oid, database)
        } else if token.keyword("replication") {
            false
        } else if let Some(regex) = &token.regex {
            regex.is_match(database)
        } else {
            token.text == database
        }
    })
}

/// One line of `pg_ident.conf`, `IdentLine`.
#[derive(Debug, Clone)]
struct IdentLine {
    map: String,
    system_user: Token,
    pg_user: Token,
}

/// The loaded `pg_ident.conf`.
#[derive(Debug, Clone, Default)]
pub(crate) struct Ident {
    lines: Vec<IdentLine>,
}

impl Ident {
    /// `load_ident`. `None` means that the file has an error, which is in `log`.
    pub(crate) fn load(path: &str, log: &mut Vec<String>) -> Option<Ident> {
        let lines = tokenize(path, log)?;
        let mut ok = true;
        let mut parsed = Vec::new();
        for line in &lines {
            if line.failed {
                ok = false;
                continue;
            }
            match parse_ident_line(line, log) {
                Some(line) => parsed.push(line),
                None => ok = false,
            }
        }
        ok.then_some(Ident { lines: parsed })
    }

    /// `check_usermap`: the system user `system_user` may log in as `pg_user` with the map
    /// `map`. A failure writes the reason to the log.
    pub(crate) fn check(
        &self,
        map: Option<&str>,
        pg_user: &str,
        system_user: &str,
        catalog: &Catalog,
    ) -> bool {
        let Some(map) = map.filter(|map| !map.is_empty()) else {
            if pg_user == system_user {
                return true;
            }
            crate::server::log(
                "LOG",
                &format!(
                    "provided user name ({pg_user}) and authenticated user name ({system_user}) \
                     do not match"
                ),
            );
            return false;
        };
        let oid = catalog.find(pg_user).map(|role| role.oid);
        for line in self.lines.iter().filter(|line| line.map == map) {
            match line.matches(pg_user, oid, system_user, catalog) {
                Some(true) => return true,
                Some(false) => {}
                None => return false,
            }
        }
        crate::server::log(
            "LOG",
            &format!(
                "no match in usermap \"{map}\" for user \"{pg_user}\" authenticated as \
                 \"{system_user}\""
            ),
        );
        false
    }
}

impl IdentLine {
    /// `check_ident_usermap` for one line of the map. `None` is an error, which is in the log.
    fn matches(
        &self,
        pg_user: &str,
        oid: Option<u32>,
        system_user: &str,
        catalog: &Catalog,
    ) -> Option<bool> {
        let Some(regex) = &self.system_user.regex else {
            if self.system_user.text != system_user {
                return Some(false);
            }
            return Some(check_role(pg_user, oid, std::slice::from_ref(&self.pg_user), catalog));
        };
        let Some(found) = regex.find_at(system_user, 0) else {
            return Some(false);
        };
        let pg = &self.pg_user;
        if pg.group().is_none() && pg.regex.is_none() && pg.text.contains("\\1") {
            let Some((start, end)) = found.group(1) else {
                crate::server::log(
                    "LOG",
                    &format!(
                        "regular expression \"{}\" has no subexpressions as requested by \
                         backreference in \"{}\"",
                        &self.system_user.text[1..],
                        pg.text
                    ),
                );
                return None;
            };
            let expanded = pg.text.replace("\\1", &system_user[start..end]);
            // The result is quoted, so it only matches as it is.
            let token = Token::new(expanded, true);
            return Some(check_role(pg_user, oid, std::slice::from_ref(&token), catalog));
        }
        Some(check_role(pg_user, oid, std::slice::from_ref(pg), catalog))
    }
}

/// `parse_ident_line`.
fn parse_ident_line(line: &TokenLine, log: &mut Vec<String>) -> Option<IdentLine> {
    let context = [line_context(line.number, &line.file)];
    let mut fail = |text: &str| {
        log.push(message(text, None, &context));
        None
    };
    let mut fields = line.fields.iter();
    let mut tokens = Vec::with_capacity(3);
    for _ in 0..3 {
        let Some(field) = fields.next() else {
            return fail("missing entry at end of line");
        };
        if field.len() > 1 {
            return fail("multiple values in ident field");
        }
        tokens.push(field[0].clone());
    }
    let mut pg_user = tokens.pop()?;
    let mut system_user = tokens.pop()?;
    let map = tokens.pop()?.text;
    for token in [&mut system_user, &mut pg_user] {
        if let Err(text) = token.compile() {
            return fail(&text);
        }
    }
    Some(IdentLine { map, system_user, pg_user })
}

/// The text of a sample file with each `@name@` put in place, as `initdb` writes it.
pub(crate) fn fill_sample(sample: &str, values: &[(&str, &str)]) -> String {
    let mut text = sample.to_owned();
    for (name, value) in values {
        text = text.replace(name, value);
    }
    text
}

/// The name of the system user with the ID `uid`, from `getpwuid_r`.
pub(crate) fn system_user_name(uid: libc::uid_t) -> Option<String> {
    let mut buf = vec![0 as libc::c_char; 16 << 10];
    // SAFETY: `passwd` and `buf` are valid for the whole call, and `found` is set by it.
    unsafe {
        let mut passwd = std::mem::zeroed::<libc::passwd>();
        let mut found: *mut libc::passwd = std::ptr::null_mut();
        let code =
            libc::getpwuid_r(uid, &raw mut passwd, buf.as_mut_ptr(), buf.len(), &raw mut found);
        if code != 0 || found.is_null() {
            return None;
        }
        Some(CStr::from_ptr(passwd.pw_name).to_string_lossy().into_owned())
    }
}

/// The absolute path of a file in the configuration directory, as the names of `hba_file` and
/// `ident_file` that PostgreSQL logs.
pub(crate) fn config_path(data: &Path, file: &Path) -> PathBuf {
    let path = if file.is_absolute() { file.to_owned() } else { data.join(file) };
    let path = std::path::absolute(&path).unwrap_or(path);
    PathBuf::from(canonical(&path))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::roles::{Member, Role};

    fn tokens(line: &str) -> Vec<Vec<(String, bool)>> {
        let mut log = Vec::new();
        let mut tokenizer = Tokenizer { log: &mut log, stack: Vec::new() };
        let mut lines = Vec::new();
        tokenizer.file("/x/pg_hba.conf", line, &mut lines, 0);
        lines
            .into_iter()
            .flat_map(|line| line.fields)
            .map(|field| field.into_iter().map(|t| (t.text, t.quoted)).collect())
            .collect()
    }

    fn plain(fields: &[&[&str]]) -> Vec<Vec<(String, bool)>> {
        fields
            .iter()
            .map(|field| field.iter().map(|t| ((*t).to_owned(), false)).collect())
            .collect()
    }

    #[test]
    fn the_tokens_of_a_line() {
        assert_eq!(tokens("host all a,b 1.2.3.4/32 md5"), {
            plain(&[&["host"], &["all"], &["a", "b"], &["1.2.3.4/32"], &["md5"]])
        });
        // A space before the comma ends the field, as in PostgreSQL.
        assert_eq!(tokens("a ,b"), plain(&[&["a"], &["b"]]));
        assert_eq!(tokens("a, b # c d"), plain(&[&["a", "b"]]));
        assert_eq!(tokens("a#b c"), plain(&[&["a"]]));
        assert_eq!(
            tokens("\"a b\" \"x\"\"y\" \"\""),
            vec![
                vec![("a b".to_owned(), true)],
                vec![("x\"y".to_owned(), true)],
                vec![(String::new(), true)],
            ]
        );
        assert_eq!(tokens("local \\\n all"), plain(&[&["local"], &["all"]]));
        assert_eq!(tokens("a\r\nb\n"), plain(&[&["a"], &["b"]]));
    }

    fn load(text: &str) -> (Option<Hba>, Vec<String>) {
        let dir = std::env::temp_dir().join(format!("rudb-hba-{}", poll_id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("pg_hba.conf");
        std::fs::write(&path, text).unwrap();
        let mut log = Vec::new();
        let path = path.to_str().unwrap().to_owned();
        let hba = Hba::load(&path, ParseSettings { ssl: false }, &mut log);
        let _ = std::fs::remove_dir_all(&dir);
        let log = log.into_iter().map(|l| l.replace(&path, "F")).collect();
        (hba, log)
    }

    fn poll_id() -> String {
        use std::sync::atomic::{AtomicUsize, Ordering};
        static NEXT: AtomicUsize = AtomicUsize::new(0);
        format!("{}-{}", std::process::id(), NEXT.fetch_add(1, Ordering::Relaxed))
    }

    #[test]
    fn the_errors_of_postgresql() {
        let cases = [
            ("foo all all trust", "invalid connection type \"foo\""),
            ("local,host all all trust", "multiple values specified for connection type"),
            ("local all", "end-of-line before role specification"),
            ("host all all", "end-of-line before IP address specification"),
            ("host all all 1.2.3.4", "end-of-line before netmask specification"),
            ("host all all 1.2.3.4 255.0.0.0", "end-of-line before authentication method"),
            ("host all all 1.2.3.4/33 trust", "invalid CIDR mask in address \"1.2.3.4/33\""),
            (
                "host all all foo/8 trust",
                "specifying both host name and CIDR mask is invalid: \"foo/8\"",
            ),
            ("host all all ::1 255.0.0.0 trust", "IP address and mask do not match"),
            ("local all all foo", "invalid authentication method \"foo\""),
            (
                "local all all ldap",
                "invalid authentication method \"ldap\": not supported by this build",
            ),
            ("host all all all peer", "peer authentication is only supported on local sockets"),
            (
                "host all all all cert",
                "cert authentication is only supported on hostssl connections",
            ),
            ("local all all trust x", "authentication option not in name=value format: x"),
            (
                "local all all trust map=x",
                "authentication option \"map\" is only valid for authentication methods ident, peer, gssapi, sspi, cert, and oauth",
            ),
            (
                "local all all trust ldapserver=x",
                "authentication option \"ldapserver\" is only valid for authentication methods ldap",
            ),
            (
                "local all all trust clientcert=verify-ca",
                "clientcert can only be configured for \"hostssl\" rows",
            ),
            ("local all all trust foo=1", "unrecognized authentication option name: \"foo\""),
            (
                "local all all oauth scope=a",
                "authentication method \"oauth\" requires argument \"issuer\" to be set",
            ),
            (
                "local all all oauth scope=a issuer=b",
                "parameter \"oauth_validator_libraries\" must be set for authentication method \"oauth\"",
            ),
        ];
        for (line, error) in cases {
            let (hba, log) = load(line);
            assert!(hba.is_none(), "{line}");
            let first = log[0].lines().next().unwrap();
            assert_eq!(first, error, "{line}");
            assert!(log[0].ends_with("CONTEXT:  line 1 of configuration file \"F\""), "{line}");
        }
        let (hba, log) = load("# nothing\n\n");
        assert!(hba.is_none());
        assert_eq!(log, ["configuration file \"F\" contains no entries"]);
        let (hba, log) = load("hostssl all all all trust\n");
        assert!(hba.is_some());
        assert_eq!(
            log,
            ["hostssl record cannot match because SSL is disabled\nHINT:  Set \"ssl = on\" in \
              postgresql.conf.\nCONTEXT:  line 1 of configuration file \"F\""]
        );
        // Every bad line goes to the log, not only the first.
        let (hba, log) = load("foo\nlocal all all trust\nbar\n");
        assert!(hba.is_none());
        assert_eq!(log.len(), 2);
        assert!(log[1].ends_with("line 3 of configuration file \"F\""));
    }

    #[test]
    fn addresses() {
        assert_eq!(numeric_host("127.1"), Some("127.0.0.1".parse().unwrap()));
        assert_eq!(numeric_host("0x7f.1"), Some("127.0.0.1".parse().unwrap()));
        assert_eq!(numeric_host("10.0.0.256"), None);
        assert_eq!(numeric_host("localhost"), None);
        assert_eq!(numeric_host("::1"), Some("::1".parse().unwrap()));
        let mask = cidr_mask("8", true).unwrap();
        assert!(in_range("10.1.2.3".parse().unwrap(), "10.0.0.0".parse().unwrap(), mask));
        assert!(!in_range("11.1.2.3".parse().unwrap(), "10.0.0.0".parse().unwrap(), mask));
        assert!(!in_range("::1".parse().unwrap(), "10.0.0.0".parse().unwrap(), mask));
        assert_eq!(cidr_mask("", true), None);
        assert_eq!(cidr_mask("1x", true), None);
        assert_eq!(cidr_mask("129", false), None);
    }

    fn catalog() -> Catalog {
        let mut catalog = Catalog::bootstrap("postgres", None);
        let mut alice = Role::new(16384, "alice");
        alice.login = true;
        catalog.roles.push(alice);
        catalog.roles.push(Role::new(16385, "staff"));
        catalog.members.push(Member {
            role: 16385,
            member: 16384,
            grantor: 10,
            admin: false,
            inherit: true,
            set: true,
        });
        catalog
    }

    #[test]
    fn the_first_line_that_matches() {
        let (hba, _) = load(
            "local sameuser all trust\n\
             local all +staff md5\n\
             host all /^b ::1/128 password\n\
             hostssl all all 127.0.0.1/32 scram-sha-256\n\
             host \"all\" all 127.0.0.0 255.0.0.0 reject\n\
             host all all 127.0.0.1/32 trust\n",
        );
        let hba = hba.unwrap();
        let catalog = catalog();
        let find = |address: Option<&str>, ssl, user, database| {
            let client = Client::new(address.map(|a| a.parse().unwrap()), ssl, user, database);
            hba.find(&client, &catalog).map(|line| line.number)
        };
        assert_eq!(find(None, false, "alice", "alice"), Some(1));
        assert_eq!(find(None, false, "alice", "db"), Some(2));
        // A superuser is not a member of every role here.
        assert_eq!(find(None, false, "postgres", "db"), None);
        assert_eq!(find(Some("::1"), false, "bob", "db"), Some(3));
        assert_eq!(find(Some("::1"), false, "alice", "db"), None);
        assert_eq!(find(Some("127.0.0.1"), true, "alice", "db"), Some(4));
        // A quoted "all" is a database name.
        assert_eq!(find(Some("127.0.0.1"), false, "alice", "all"), Some(5));
        assert_eq!(find(Some("127.0.0.1"), false, "alice", "db"), Some(6));
    }

    #[test]
    fn ident_maps() {
        let dir = std::env::temp_dir().join(format!("rudb-ident-{}", poll_id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("pg_ident.conf");
        std::fs::write(&path, "m tom alice\nm /^(.*)@example\\.com$ \\1\nm /^x +staff\n").unwrap();
        let mut log = Vec::new();
        let ident = Ident::load(path.to_str().unwrap(), &mut log).unwrap();
        let _ = std::fs::remove_dir_all(&dir);
        let catalog = catalog();
        assert!(ident.check(Some("m"), "alice", "tom", &catalog));
        assert!(ident.check(Some("m"), "alice", "alice@example.com", &catalog));
        assert!(!ident.check(Some("m"), "bob", "alice@example.com", &catalog));
        assert!(ident.check(Some("m"), "alice", "xavier", &catalog));
        assert!(ident.check(None, "alice", "alice", &catalog));
        assert!(!ident.check(None, "alice", "tom", &catalog));
    }

    #[test]
    fn includes() {
        let dir = std::env::temp_dir().join(format!("rudb-include-{}", poll_id()));
        std::fs::create_dir_all(dir.join("conf.d")).unwrap();
        std::fs::write(dir.join("users"), "alice\nbob, carol\n").unwrap();
        std::fs::write(dir.join("conf.d/b.conf"), "local all all reject\n").unwrap();
        std::fs::write(dir.join("conf.d/a.conf"), "local db all trust\n").unwrap();
        std::fs::write(dir.join("conf.d/.c.conf"), "bad\n").unwrap();
        std::fs::write(
            dir.join("pg_hba.conf"),
            "local all @users md5\ninclude_dir conf.d\ninclude_if_exists none\n",
        )
        .unwrap();
        let path = dir.join("pg_hba.conf");
        let mut log = Vec::new();
        let hba = Hba::load(path.to_str().unwrap(), ParseSettings { ssl: false }, &mut log);
        let hba = hba.unwrap();
        let roles: Vec<&str> = hba.lines[0].roles.iter().map(|t| t.text.as_str()).collect();
        assert_eq!(roles, ["alice", "bob", "carol"]);
        assert_eq!(hba.lines[1].method, Method::Trust);
        assert_eq!(hba.lines[2].method, Method::Reject);
        assert!(hba.lines[1].file.ends_with("conf.d/a.conf"));
        // PostgreSQL logs the failed open first, then the skip.
        assert_eq!(log.len(), 2, "{log:?}");
        assert!(log[0].starts_with("could not open file"), "{log:?}");
        assert!(log[1].starts_with("skipping missing authentication file"), "{log:?}");
        let context = format!("line 3 of configuration file \"{}\"", path.display());
        assert!(log[1].ends_with(&context), "{log:?}");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
