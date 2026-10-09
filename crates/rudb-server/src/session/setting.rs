//! The `SET`, `RESET` and `SHOW` statements of PostgreSQL, which the server runs itself.
//!
//! The session keeps the values of the parameters of PostgreSQL in [`Settings`], so the server
//! reads these statements with the grammar of `gram.y` and does not give them to the engine. A
//! statement for a name that is not a parameter of PostgreSQL and has no dot, such as
//! `SET threads = 4`, goes to the engine, which has settings of its own. A statement that this
//! reader cannot read also goes to the engine, which gives the syntax error.

use rudb_common::guc::{self, Arg, Settings};
use rudb_pgtypes::keywords::{Category, category};

use super::role::{Invalid, Notices, failure, notice};
use super::{Failure, Severity, database, role};
use crate::db_role_settings::{DbRoleSettings, split};
use crate::roles;

/// A statement that the server runs on the settings of the session.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum Command {
    /// `SET name TO value`, `SET name TO DEFAULT` with `value` `None`, and `RESET name` with
    /// `reset` true.
    Set { name: String, value: Option<Vec<Arg>>, local: bool, reset: bool },
    /// `RESET ALL`.
    ResetAll,
    /// `SHOW name`.
    Show(String),
    /// `SHOW ALL`.
    ShowAll,
    /// `SET SESSION AUTHORIZATION` and `RESET SESSION AUTHORIZATION`, with `None` for `DEFAULT`.
    Authorization { user: Option<String>, reset: bool },
    /// `SET ROLE`.
    Role(String),
    /// `CREATE ROLE`, `ALTER ROLE` and `DROP ROLE`, and their forms with `USER` and `GROUP`.
    Roles(role::Parsed),
    /// A statement on the databases.
    Databases(database::Parsed),
}

/// A token of the statement.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum Token {
    /// A name or a key word. `quoted` is true for a name in double quotes, which keeps its case
    /// and is never a key word.
    Word { text: String, quoted: bool },
    /// A string constant, after the escapes.
    String(String),
    /// A number, as written. `integer` is true for one without a fraction or an exponent.
    Number { text: String, integer: bool },
    /// One character of punctuation.
    Punct(char),
}

impl Token {
    /// Whether the token is the key word `word`, in lower case.
    pub(super) fn is(&self, word: &str) -> bool {
        matches!(self, Token::Word { text, quoted: false } if text == word)
    }
}

/// Splits the statement into tokens. `None` for a token that this reader does not know, such as
/// a `U&` string, which leaves the statement to the engine.
fn tokens(sql: &str) -> Option<Vec<Token>> {
    spanned(sql).map(|(tokens, _)| tokens)
}

/// The tokens of the statement and the byte offset where each one starts.
pub(super) fn spanned(sql: &str) -> Option<(Vec<Token>, Vec<usize>)> {
    scan(sql, false)
}

/// The tokens of a statement that this reader only looks at, such as a query: other punctuation
/// is a token of its own, and a bit string or a `U&` string is not an error.
pub(super) fn loose(sql: &str) -> Option<Vec<Token>> {
    scan(sql, true).map(|(tokens, _)| tokens)
}

/// The tokens of [`loose`] and the byte offset where each one starts.
pub(super) fn loose_spanned(sql: &str) -> Option<(Vec<Token>, Vec<usize>)> {
    scan(sql, true)
}

fn scan(sql: &str, loose: bool) -> Option<(Vec<Token>, Vec<usize>)> {
    let bytes = sql.as_bytes();
    let mut tokens = Vec::new();
    let mut starts = Vec::new();
    let mut i = 0;
    while i < bytes.len() {
        let (c, at, count) = (bytes[i], i, tokens.len());
        if c.is_ascii_whitespace() {
            i += 1;
        } else if sql[i..].starts_with("--") {
            i = sql[i..].find('\n').map_or(bytes.len(), |end| i + end);
        } else if sql[i..].starts_with("/*") {
            let mut depth = 0;
            loop {
                if sql[i..].starts_with("/*") {
                    depth += 1;
                    i += 2;
                } else if sql[i..].starts_with("*/") {
                    depth -= 1;
                    i += 2;
                    if depth == 0 {
                        break;
                    }
                } else if i >= bytes.len() {
                    return None;
                } else {
                    i += 1;
                }
            }
        } else if (c == b'E' || c == b'e') && bytes.get(i + 1) == Some(&b'\'') {
            let (text, end) = escaped_string(sql, i + 2)?;
            tokens.push(Token::String(text));
            i = end;
        } else if (c == b'N' || c == b'n') && bytes.get(i + 1) == Some(&b'\'') {
            let (text, end) = string(sql, i + 2)?;
            tokens.push(Token::String(text));
            i = end;
        } else if c == b'\'' {
            let (text, end) = string(sql, i + 1)?;
            tokens.push(Token::String(text));
            i = end;
        } else if c == b'"' {
            let mut text = String::new();
            let mut j = i + 1;
            loop {
                let end = j + sql[j..].find('"')?;
                text.push_str(&sql[j..end]);
                if bytes.get(end + 1) == Some(&b'"') {
                    text.push('"');
                    j = end + 2;
                } else {
                    i = end + 1;
                    break;
                }
            }
            if text.is_empty() {
                return None;
            }
            tokens.push(Token::Word { text, quoted: true });
        } else if c == b'$' {
            let tag_end = i + 1 + sql[i + 1..].find('$')?;
            let tag = &sql[i..=tag_end];
            if !tag[1..tag.len() - 1]
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b >= 0x80)
                || tag[1..].starts_with(|c: char| c.is_ascii_digit())
            {
                return None;
            }
            let body = tag_end + 1;
            let end = body + sql[body..].find(tag)?;
            tokens.push(Token::String(sql[body..end].to_owned()));
            i = end + tag.len();
        } else if c.is_ascii_digit()
            || (c == b'.' && bytes.get(i + 1).is_some_and(u8::is_ascii_digit))
        {
            let start = i;
            let mut integer = true;
            while bytes.get(i).is_some_and(u8::is_ascii_digit) {
                i += 1;
            }
            if bytes.get(i) == Some(&b'.') {
                integer = false;
                i += 1;
                while bytes.get(i).is_some_and(u8::is_ascii_digit) {
                    i += 1;
                }
            }
            if matches!(bytes.get(i), Some(b'e' | b'E')) {
                let mut j = i + 1;
                if matches!(bytes.get(j), Some(b'+' | b'-')) {
                    j += 1;
                }
                if bytes.get(j).is_some_and(u8::is_ascii_digit) {
                    integer = false;
                    i = j;
                    while bytes.get(i).is_some_and(u8::is_ascii_digit) {
                        i += 1;
                    }
                }
            }
            if !loose && bytes.get(i).is_some_and(|b| b.is_ascii_alphabetic() || *b == b'_') {
                return None;
            }
            tokens.push(Token::Number { text: sql[start..i].to_owned(), integer });
        } else if c.is_ascii_alphabetic() || c == b'_' || c >= 0x80 {
            let start = i;
            while i < bytes.len()
                && (bytes[i].is_ascii_alphanumeric()
                    || bytes[i] == b'_'
                    || bytes[i] == b'$'
                    || bytes[i] >= 0x80)
            {
                i += 1;
            }
            if !loose && (bytes.get(i) == Some(&b'\'') || bytes.get(i) == Some(&b'&')) {
                // `B'...'`, `X'...'` and `U&'...'`.
                return None;
            }
            tokens.push(Token::Word { text: sql[start..i].to_ascii_lowercase(), quoted: false });
        } else if loose || matches!(c, b'=' | b',' | b'.' | b'(' | b')' | b'+' | b'-' | b';') {
            tokens.push(Token::Punct(char::from(c)));
            i += 1;
        } else {
            return None;
        }
        if tokens.len() > count {
            starts.push(at);
        }
    }
    Some((tokens, starts))
}

/// A string in single quotes from `start`, after the quote, with a doubled quote for one. The
/// text and the place after the closing quote.
fn string(sql: &str, start: usize) -> Option<(String, usize)> {
    let mut text = String::new();
    let mut i = start;
    loop {
        let end = i + sql[i..].find('\'')?;
        text.push_str(&sql[i..end]);
        if sql.as_bytes().get(end + 1) == Some(&b'\'') {
            text.push('\'');
            i = end + 2;
        } else {
            return Some((text, end + 1));
        }
    }
}

/// A string of the form `E'...'` from `start`, after the quote, with the backslash escapes of
/// PostgreSQL.
fn escaped_string(sql: &str, start: usize) -> Option<(String, usize)> {
    let mut bytes = Vec::new();
    let mut chars = sql[start..].char_indices().peekable();
    let digits =
        |chars: &mut std::iter::Peekable<std::str::CharIndices<'_>>, radix: u32, max: usize| {
            let mut value = 0u32;
            let mut n = 0;
            while n < max
                && let Some(digit) = chars.peek().and_then(|(_, c)| c.to_digit(radix))
            {
                value = value * radix + digit;
                chars.next();
                n += 1;
            }
            (value, n)
        };
    while let Some((at, c)) = chars.next() {
        match c {
            '\'' if chars.peek().map(|(_, c)| *c) == Some('\'') => {
                chars.next();
                bytes.push(b'\'');
            }
            '\'' => return Some((String::from_utf8(bytes).ok()?, start + at + 1)),
            '\\' => {
                let (_, e) = chars.next()?;
                match e {
                    'b' => bytes.push(8),
                    'f' => bytes.push(12),
                    'n' => bytes.push(b'\n'),
                    'r' => bytes.push(b'\r'),
                    't' => bytes.push(b'\t'),
                    '0'..='7' => {
                        let (rest, n) = digits(&mut chars, 8, 2);
                        let first = e.to_digit(8)?;
                        bytes.push(u8::try_from((first << (3 * n)) | rest).ok()?);
                    }
                    'x' => {
                        let (value, n) = digits(&mut chars, 16, 2);
                        if n == 0 {
                            bytes.push(b'x');
                        } else {
                            bytes.push(u8::try_from(value).ok()?);
                        }
                    }
                    'u' | 'U' => {
                        let (value, n) = digits(&mut chars, 16, if e == 'u' { 4 } else { 8 });
                        if n != if e == 'u' { 4 } else { 8 } {
                            return None;
                        }
                        let mut buf = [0u8; 4];
                        bytes.extend_from_slice(
                            char::from_u32(value)?.encode_utf8(&mut buf).as_bytes(),
                        );
                    }
                    other => {
                        let mut buf = [0u8; 4];
                        bytes.extend_from_slice(other.encode_utf8(&mut buf).as_bytes());
                    }
                }
            }
            other => {
                let mut buf = [0u8; 4];
                bytes.extend_from_slice(other.encode_utf8(&mut buf).as_bytes());
            }
        }
    }
    None
}

/// Reads a statement. `None` when it is not a statement that the server runs.
pub(super) fn parse(sql: &str) -> Option<Command> {
    let head = sql.trim_start().as_bytes();
    let starts = |word: &str| {
        head.get(..word.len()).is_some_and(|h| h.eq_ignore_ascii_case(word.as_bytes()))
    };
    if starts("create") || starts("alter") || starts("drop") {
        return database::parse(sql)
            .map(Command::Databases)
            .or_else(|| role::parse(sql).map(Command::Roles));
    }
    if !["set", "reset", "show"].iter().any(|w| starts(w)) {
        return None;
    }
    let mut tokens = tokens(sql)?;
    while tokens.last() == Some(&Token::Punct(';')) {
        tokens.pop();
    }
    let mut p = Parser { tokens, at: 0 };
    let command = if p.eat("set") {
        p.set()?
    } else if p.eat("reset") {
        p.reset()?
    } else if p.eat("show") {
        p.show()?
    } else {
        return None;
    };
    if p.at != p.tokens.len() {
        return None;
    }
    let name = match &command {
        Command::Set { name, .. } | Command::Show(name) => name,
        _ => return Some(command),
    };
    // A name that PostgreSQL does not have and that is not a placeholder goes to the engine.
    (guc::find(name).is_some() || name.contains('.')).then_some(command)
}

struct Parser {
    tokens: Vec<Token>,
    at: usize,
}

impl Parser {
    fn peek(&self, ahead: usize) -> Option<&Token> {
        self.tokens.get(self.at + ahead)
    }

    /// Takes the key word `word` if it is next.
    fn eat(&mut self, word: &str) -> bool {
        let next = self.peek(0).is_some_and(|t| t.is(word));
        if next {
            self.at += 1;
        }
        next
    }

    /// Takes the key words `words` if they are next.
    fn eat_all(&mut self, words: &[&str]) -> bool {
        let next = words.iter().enumerate().all(|(i, w)| self.peek(i).is_some_and(|t| t.is(w)));
        if next {
            self.at += words.len();
        }
        next
    }

    fn punct(&mut self, c: char) -> bool {
        let next = self.peek(0) == Some(&Token::Punct(c));
        if next {
            self.at += 1;
        }
        next
    }

    fn next(&mut self) -> Option<Token> {
        let token = self.tokens.get(self.at).cloned();
        self.at += 1;
        token
    }

    fn string(&mut self) -> Option<String> {
        match self.next()? {
            Token::String(text) => Some(text),
            _ => None,
        }
    }

    /// `NonReservedWord_or_Sconst`.
    fn word_or_string(&mut self) -> Option<String> {
        match self.next()? {
            Token::String(text) | Token::Word { text, .. } => Some(text),
            Token::Number { .. } | Token::Punct(_) => None,
        }
    }

    /// `var_name`: names with dots between them.
    fn var_name(&mut self) -> Option<String> {
        let Token::Word { mut text, .. } = self.next()? else { return None };
        while self.punct('.') {
            let Token::Word { text: part, .. } = self.next()? else { return None };
            text.push('.');
            text.push_str(&part);
        }
        Some(text)
    }

    /// `SET [SESSION | LOCAL] set_rest`.
    fn set(&mut self) -> Option<Command> {
        let local = if self.eat("local") {
            true
        } else {
            // `SET SESSION AUTHORIZATION` and `SET SESSION CHARACTERISTICS` keep the word.
            if self.peek(0).is_some_and(|t| t.is("session"))
                && !self.peek(1).is_some_and(|t| t.is("authorization") || t.is("characteristics"))
            {
                self.at += 1;
            }
            false
        };
        let set = |name: &str, value: Option<Vec<Arg>>| {
            Some(Command::Set { name: name.to_owned(), value, local, reset: false })
        };
        if self.eat_all(&["time", "zone"]) {
            return set("timezone", self.zone_value()?);
        }
        if self.eat_all(&["session", "authorization"]) {
            if self.eat("default") {
                return Some(Command::Authorization { user: None, reset: false });
            }
            return Some(Command::Authorization {
                user: Some(self.word_or_string()?),
                reset: false,
            });
        }
        if self.eat("schema") {
            return set("search_path", Some(vec![Arg::String(self.string()?)]));
        }
        if self.eat("names") {
            if self.peek(0).is_none() || self.eat("default") {
                return set("client_encoding", None);
            }
            return set("client_encoding", Some(vec![Arg::String(self.string()?)]));
        }
        if self.eat("role") {
            return Some(Command::Role(self.word_or_string()?));
        }
        if self.eat_all(&["xml", "option"]) {
            let option = if self.eat("document") {
                "document"
            } else if self.eat("content") {
                "content"
            } else {
                return None;
            };
            return set("xmloption", Some(vec![Arg::String(option.to_owned())]));
        }
        if self.peek(0).is_some_and(|t| t.is("transaction") || t.is("catalog"))
            || self.peek(1).is_some_and(|t| t.is("characteristics"))
        {
            return None;
        }
        let name = self.var_name()?;
        if !self.eat("to") && !self.punct('=') {
            return None;
        }
        if self.eat("default") {
            return set(&name, None);
        }
        let mut values = vec![self.var_value()?];
        while self.punct(',') {
            values.push(self.var_value()?);
        }
        set(&name, Some(values))
    }

    /// `var_value`: a string, a name, a key word, or a number with a sign.
    fn var_value(&mut self) -> Option<Arg> {
        let negative = if self.punct('-') {
            true
        } else {
            self.punct('+');
            false
        };
        match self.next()? {
            Token::Number { text, integer } => {
                let text = if negative { format!("-{text}") } else { text };
                Some(if integer { Arg::Integer(text) } else { Arg::Number(text) })
            }
            _ if negative => None,
            Token::String(text) | Token::Word { text, .. } => Some(Arg::String(text)),
            Token::Punct(_) => None,
        }
    }

    /// `zone_value` of `SET TIME ZONE`. `None` inside for `LOCAL` and `DEFAULT`.
    #[allow(clippy::option_option)]
    fn zone_value(&mut self) -> Option<Option<Vec<Arg>>> {
        if self.eat("local") || self.eat("default") {
            return Some(None);
        }
        if self.eat("interval") {
            if self.punct('(') {
                let Token::Number { integer: true, .. } = self.next()? else { return None };
                if !self.punct(')') {
                    return None;
                }
            }
            let text = self.string()?;
            // `opt_interval`, which can only be `HOUR` or `HOUR TO MINUTE` here.
            if !self.eat_all(&["hour", "to", "minute"]) {
                self.eat("hour");
            }
            return Some(Some(vec![Arg::Interval(text)]));
        }
        Some(Some(vec![self.var_value()?]))
    }

    /// `RESET name`, `RESET ALL` and the special forms.
    fn reset(&mut self) -> Option<Command> {
        let reset = |name: &str| {
            Some(Command::Set { name: name.to_owned(), value: None, local: false, reset: true })
        };
        if self.eat("all") {
            Some(Command::ResetAll)
        } else if self.eat_all(&["time", "zone"]) {
            reset("timezone")
        } else if self.eat_all(&["transaction", "isolation", "level"]) {
            reset("transaction_isolation")
        } else if self.eat_all(&["session", "authorization"]) {
            Some(Command::Authorization { user: None, reset: true })
        } else {
            reset(&self.var_name()?)
        }
    }

    /// `SHOW name`, `SHOW ALL` and the special forms.
    fn show(&mut self) -> Option<Command> {
        let name = if self.eat("all") {
            return Some(Command::ShowAll);
        } else if self.eat_all(&["time", "zone"]) {
            "timezone".to_owned()
        } else if self.eat_all(&["transaction", "isolation", "level"]) {
            "transaction_isolation".to_owned()
        } else if self.eat_all(&["session", "authorization"]) {
            "session_authorization".to_owned()
        } else {
            self.var_name()?
        };
        Some(Command::Show(name))
    }
}

/// The `SetResetClause` of `ALTER DATABASE` and `ALTER ROLE`, as `ExtractSetVariableArgs` sees it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum Clause {
    /// `SET name TO value`. `value` is `None` for the forms that remove a value, such as
    /// `SET name TO DEFAULT` and `RESET name`, and for the forms with more than one value, which
    /// have the name in upper case, such as `TRANSACTION`.
    Set { name: String, value: Option<Vec<Arg>> },
    /// `SET name FROM CURRENT`, which keeps the value of the session.
    Current(String),
    /// `RESET ALL`.
    ResetAll,
}

/// Reads a `SetResetClause` at the next token, with the rules of `set_rest` and `reset_rest` in
/// `gram.y`. A key word that starts a special form, such as `ROLE`, is a name when `TO`, `=` or
/// `FROM` comes after it.
pub(super) fn clause(p: &mut role::Parser<'_>) -> Result<Clause, Invalid> {
    let set =
        |name: &str, value: Option<Vec<Arg>>| Ok(Clause::Set { name: name.to_owned(), value });
    if p.eat("reset") {
        if p.eat("all") {
            return Ok(Clause::ResetAll);
        }
        if p.eat("time") {
            p.expect("zone")?;
            return set("timezone", None);
        }
        if p.eat("transaction") {
            p.expect("isolation")?;
            p.expect("level")?;
            return set("transaction_isolation", None);
        }
        if p.peek(0).is_some_and(|t| t.is("session"))
            && p.peek(1).is_some_and(|t| t.is("authorization"))
        {
            p.at += 2;
            return set("session_authorization", None);
        }
        return set(&var_name(p)?, None);
    }
    p.expect("set")?;
    let generic = p.peek(1).is_some_and(|t| {
        t.is("to") || t.is("from") || *t == Token::Punct('=') || *t == Token::Punct('.')
    });
    let special = |word: &str| p.peek(0).is_some_and(|t| t.is(word));
    if special("session") && p.peek(1).is_some_and(|t| t.is("characteristics")) {
        p.at += 2;
        p.expect("as")?;
        p.expect("transaction")?;
        transaction_modes(p)?;
        return set("SESSION CHARACTERISTICS", None);
    }
    if special("session") && p.peek(1).is_some_and(|t| t.is("authorization")) {
        p.at += 2;
        if p.eat("default") {
            return set("session_authorization", None);
        }
        return set("session_authorization", Some(vec![Arg::String(word_or_string(p)?)]));
    }
    if !generic {
        if p.eat("transaction") {
            if p.eat("snapshot") {
                p.string()?;
                return set("TRANSACTION SNAPSHOT", None);
            }
            transaction_modes(p)?;
            return set("TRANSACTION", None);
        }
        if p.eat("time") {
            p.expect("zone")?;
            return set("timezone", zone_value(p)?);
        }
        if p.eat("catalog") {
            let at = p.here();
            p.string()?;
            return Err(Invalid {
                sqlstate: "0A000",
                message: "current database cannot be changed".to_owned(),
                hint: None,
                at,
            });
        }
        if p.eat("schema") {
            return set("search_path", Some(vec![Arg::String(p.string()?)]));
        }
        if p.eat("names") {
            if p.done() || p.eat("default") {
                return set("client_encoding", None);
            }
            return set("client_encoding", Some(vec![Arg::String(p.string()?)]));
        }
        if p.eat("role") {
            return set("role", Some(vec![Arg::String(word_or_string(p)?)]));
        }
        if p.eat("xml") {
            p.expect("option")?;
            let option = if p.eat("document") {
                "document"
            } else {
                p.expect("content")?;
                "content"
            };
            return set("xmloption", Some(vec![Arg::String(option.to_owned())]));
        }
    }
    let name = var_name(p)?;
    if p.eat("from") {
        p.expect("current")?;
        return Ok(Clause::Current(name));
    }
    if !p.eat("to") && !eat_punct(p, '=') {
        return Err(p.syntax());
    }
    if p.eat("default") {
        return set(&name, None);
    }
    let mut values = vec![var_value(p)?];
    while eat_punct(p, ',') {
        values.push(var_value(p)?);
    }
    set(&name, Some(values))
}

fn eat_punct(p: &mut role::Parser<'_>, c: char) -> bool {
    let next = p.peek(0) == Some(&Token::Punct(c));
    if next {
        p.at += 1;
    }
    next
}

/// Whether a word that is not in quotes is a key word of `category`, or no key word.
fn is_word(text: &str, allowed: &[Category]) -> bool {
    category(text).is_none_or(|c| allowed.contains(&c))
}

/// `var_name`: `ColId`, with dots between them.
fn var_name(p: &mut role::Parser<'_>) -> Result<String, Invalid> {
    let mut name = String::new();
    loop {
        match p.peek(0) {
            Some(Token::Word { text, quoted })
                if *quoted || is_word(text, &[Category::Unreserved, Category::ColName]) =>
            {
                name.push_str(text);
                p.at += 1;
            }
            _ => return Err(p.syntax()),
        }
        if !eat_punct(p, '.') {
            return Ok(name);
        }
        name.push('.');
    }
}

/// `NonReservedWord_or_Sconst`.
fn word_or_string(p: &mut role::Parser<'_>) -> Result<String, Invalid> {
    match p.peek(0) {
        Some(Token::String(text)) => {
            let text = text.clone();
            p.at += 1;
            Ok(text)
        }
        Some(Token::Word { text, quoted })
            if *quoted
                || is_word(
                    text,
                    &[Category::Unreserved, Category::ColName, Category::TypeFuncName],
                ) =>
        {
            let text = text.clone();
            p.at += 1;
            Ok(text)
        }
        _ => Err(p.syntax()),
    }
}

/// `var_value`: `opt_boolean_or_string` or `NumericOnly`.
fn var_value(p: &mut role::Parser<'_>) -> Result<Arg, Invalid> {
    match p.peek(0) {
        Some(Token::Word { text, quoted: false })
            if matches!(text.as_str(), "true" | "false" | "on") =>
        {
            let text = text.clone();
            p.at += 1;
            Ok(Arg::String(text))
        }
        Some(Token::String(_) | Token::Word { .. }) => Ok(Arg::String(word_or_string(p)?)),
        _ => numeric(p),
    }
}

/// `NumericOnly`: a number with an optional sign.
fn numeric(p: &mut role::Parser<'_>) -> Result<Arg, Invalid> {
    let negative = if eat_punct(p, '-') {
        true
    } else {
        eat_punct(p, '+');
        false
    };
    let Some(Token::Number { text, integer }) = p.peek(0) else { return Err(p.syntax()) };
    let text = if negative { format!("-{text}") } else { text.clone() };
    let integer = *integer;
    p.at += 1;
    Ok(if integer { Arg::Integer(text) } else { Arg::Number(text) })
}

/// `zone_value`. `None` for `LOCAL` and `DEFAULT`.
fn zone_value(p: &mut role::Parser<'_>) -> Result<Option<Vec<Arg>>, Invalid> {
    if p.eat("local") || p.eat("default") {
        return Ok(None);
    }
    if p.eat("interval") {
        if eat_punct(p, '(') {
            if !matches!(p.peek(0), Some(Token::Number { integer: true, .. })) {
                return Err(p.syntax());
            }
            p.at += 1;
            if !eat_punct(p, ')') {
                return Err(p.syntax());
            }
        }
        let text = p.string()?;
        if p.eat("hour") && p.eat("to") {
            p.expect("minute")?;
        }
        return Ok(Some(vec![Arg::Interval(text)]));
    }
    match p.peek(0) {
        Some(Token::String(text)) => {
            let text = text.clone();
            p.at += 1;
            Ok(Some(vec![Arg::String(text)]))
        }
        Some(Token::Word { text, quoted }) if *quoted || category(text).is_none() => {
            let text = text.clone();
            p.at += 1;
            Ok(Some(vec![Arg::String(text)]))
        }
        _ => Ok(Some(vec![numeric(p)?])),
    }
}

/// `transaction_mode_list`, whose items the clause does not keep.
fn transaction_modes(p: &mut role::Parser<'_>) -> Result<(), Invalid> {
    loop {
        if p.eat("isolation") {
            p.expect("level")?;
            if p.eat("read") {
                if !p.eat("uncommitted") {
                    p.expect("committed")?;
                }
            } else if p.eat("repeatable") {
                p.expect("read")?;
            } else {
                p.expect("serializable")?;
            }
        } else if p.eat("read") {
            if !p.eat("only") {
                p.expect("write")?;
            }
        } else {
            // `[NOT] DEFERRABLE`.
            p.eat("not");
            p.expect("deferrable")?;
        }
        if p.done() {
            return Ok(());
        }
        eat_punct(p, ',');
    }
}

/// What `AlterSetting` needs from the session.
pub(super) struct Keep<'a> {
    pub(super) settings: &'a DbRoleSettings,
    pub(super) guc: &'a Settings,
    pub(super) roles: &'a roles::Catalog,
    /// The session user, `GetSessionUserId`, for the check of a value of `role`.
    pub(super) session: u32,
}

/// `AlterSetting`: keeps the value of a clause in the row of `database` and `role` of
/// `pg_db_role_setting`, after the check of `validate_option_array_item`. The check of a value of
/// `role` or `session_authorization` gives a `NOTICE` and not an error, as `check_role` does for
/// the source `PGC_S_TEST`.
pub(super) fn keep(
    clause: &Clause,
    database: u32,
    role: u32,
    cx: &Keep<'_>,
    out: &mut Notices<'_>,
) -> Result<(), Failure> {
    let engine = |e: rudb::Error| Failure::engine(&e, 0);
    let written = |e: String| failure("XX000", e);
    let (name, text) = match clause {
        Clause::ResetAll => {
            let superuser = cx.guc.superuser();
            return cx.settings.change(
                |catalog| {
                    catalog.retain(database, role, |item| {
                        !superuser
                            && split(item).is_none_or(|(name, _)| !cx.guc.stored_resettable(name))
                    });
                    Ok(())
                },
                written,
            );
        }
        Clause::Current(name) => (name, Some(cx.guc.show(name).map_err(engine)?.1)),
        Clause::Set { name, value: Some(args) } => {
            (name, Some(guc::flatten(name, args).map_err(engine)?))
        }
        Clause::Set { name, value: None } => (name, None),
    };
    let key = cx.guc.check_stored(name, text.as_deref()).map_err(engine)?;
    let Some(text) = text else {
        return cx.settings.change(
            |catalog| {
                catalog.delete(database, role, &key);
                Ok(())
            },
            written,
        );
    };
    match key.as_str() {
        "role" if text != "none" => match cx.roles.find(&text) {
            None => notice(
                out,
                Severity::Notice,
                "42704",
                &format!("role \"{text}\" does not exist"),
                &[],
            ),
            Some(target) if !cx.roles.can_set(cx.session, target.oid) => notice(
                out,
                Severity::Notice,
                "42501",
                &format!("permission will be denied to set role \"{text}\""),
                &[],
            ),
            Some(_) => {}
        },
        "session_authorization" if cx.roles.find(&text).is_none() => {
            notice(out, Severity::Notice, "42704", &format!("role \"{text}\" does not exist"), &[]);
        }
        _ => {}
    }
    cx.settings.change(
        |catalog| {
            catalog.add(database, role, &key, &text);
            Ok(())
        },
        written,
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn set(name: &str, value: Option<Vec<Arg>>, local: bool) -> Option<Command> {
        Some(Command::Set { name: name.to_owned(), value, local, reset: false })
    }

    fn s(text: &str) -> Arg {
        Arg::String(text.to_owned())
    }

    fn clause_of(sql: &str) -> Result<Clause, String> {
        let (tokens, starts) = spanned(sql).unwrap();
        let mut p = role::Parser { sql, tokens, starts, at: 0 };
        let clause = clause(&mut p).map_err(|invalid| invalid.message)?;
        if !p.done() {
            return Err(format!("{sql} has more tokens"));
        }
        Ok(clause)
    }

    #[test]
    fn the_clauses_of_alter_database_and_alter_role() {
        let set =
            |name: &str, value: Option<Vec<Arg>>| Ok(Clause::Set { name: name.to_owned(), value });
        assert_eq!(clause_of("set work_mem to default"), set("work_mem", None));
        assert_eq!(clause_of("set a.b.c = 1"), set("a.b.c", Some(vec![Arg::Integer("1".into())])));
        assert_eq!(clause_of("set work_mem from current"), Ok(Clause::Current("work_mem".into())));
        assert_eq!(clause_of("reset all"), Ok(Clause::ResetAll));
        assert_eq!(clause_of("reset time zone"), set("timezone", None));
        assert_eq!(clause_of("reset session authorization"), set("session_authorization", None));
        assert_eq!(
            clause_of("reset transaction isolation level"),
            set("transaction_isolation", None)
        );
        assert_eq!(clause_of("set time zone local"), set("timezone", None));
        assert_eq!(
            clause_of("set time zone interval '+02:00' hour to minute"),
            set("timezone", Some(vec![Arg::Interval("+02:00".into())]))
        );
        assert_eq!(clause_of("set schema 'a'"), set("search_path", Some(vec![s("a")])));
        assert_eq!(clause_of("set names"), set("client_encoding", None));
        assert_eq!(clause_of("set names default"), set("client_encoding", None));
        assert_eq!(clause_of("set names = 'x'"), set("names", Some(vec![s("x")])));
        assert_eq!(clause_of("set role x"), set("role", Some(vec![s("x")])));
        assert_eq!(clause_of("set role = 'x'"), set("role", Some(vec![s("x")])));
        assert_eq!(
            clause_of("set session authorization default"),
            set("session_authorization", None)
        );
        assert_eq!(
            clause_of("set xml option document"),
            set("xmloption", Some(vec![s("document")]))
        );
        assert_eq!(
            clause_of("set transaction isolation level read committed, read only"),
            set("TRANSACTION", None)
        );
        assert_eq!(
            clause_of("set session characteristics as transaction read write"),
            set("SESSION CHARACTERISTICS", None)
        );
        assert_eq!(
            clause_of("set search_path = a, \"B\", 'c d'"),
            set("search_path", Some(vec![s("a"), s("B"), s("c d")]))
        );
        assert_eq!(clause_of("set catalog 'x'"), Err("current database cannot be changed".into()));
    }

    #[test]
    fn set_statements() {
        assert_eq!(parse("set timezone = 'utc'"), set("timezone", Some(vec![s("utc")]), false));
        assert_eq!(
            parse("SET LOCAL DateStyle TO iso, MDY"),
            set("datestyle", Some(vec![s("iso"), s("mdy")]), true)
        );
        assert_eq!(
            parse("set \"TimeZone\" = 'UTC';"),
            set("TimeZone", Some(vec![s("UTC")]), false)
        );
        assert_eq!(
            parse("set search_path = \"My Schema\", public, 'x y', $$z$$"),
            set("search_path", Some(vec![s("My Schema"), s("public"), s("x y"), s("z")]), false)
        );
        assert_eq!(
            parse("set extra_float_digits = -3"),
            set("extra_float_digits", Some(vec![Arg::Integer("-3".into())]), false)
        );
        assert_eq!(
            parse("set seed = .5"),
            set("seed", Some(vec![Arg::Number(".5".into())]), false)
        );
        assert_eq!(parse("set session jit to default"), set("jit", None, false));
        assert_eq!(
            parse("set myapp.user_id = 42"),
            set("myapp.user_id", Some(vec![Arg::Integer("42".into())]), false)
        );
        assert_eq!(
            parse("set application_name = E'a\\tb'"),
            set("application_name", Some(vec![s("a\tb")]), false)
        );
        assert_eq!(parse("set threads = 4"), None);
        assert_eq!(parse("set transaction isolation level serializable"), None);
        assert_eq!(parse("set session characteristics as transaction read only"), None);
        assert_eq!(parse("select 1"), None);
    }

    #[test]
    fn special_forms() {
        assert_eq!(parse("set time zone local"), set("timezone", None, false));
        assert_eq!(
            parse("set time zone interval '+05:30' hour to minute"),
            set("timezone", Some(vec![Arg::Interval("+05:30".into())]), false)
        );
        assert_eq!(
            parse("set time zone -3"),
            set("timezone", Some(vec![Arg::Integer("-3".into())]), false)
        );
        assert_eq!(parse("set names 'utf8'"), set("client_encoding", Some(vec![s("utf8")]), false));
        assert_eq!(parse("set schema 'abc'"), set("search_path", Some(vec![s("abc")]), false));
        assert_eq!(
            parse("set session authorization default"),
            Some(Command::Authorization { user: None, reset: false })
        );
        assert_eq!(parse("set role none"), Some(Command::Role("none".into())));
        assert_eq!(parse("reset all"), Some(Command::ResetAll));
        assert_eq!(
            parse("reset transaction isolation level"),
            Some(Command::Set {
                name: "transaction_isolation".into(),
                value: None,
                local: false,
                reset: true
            })
        );
        assert_eq!(parse("show all"), Some(Command::ShowAll));
        assert_eq!(parse("SHOW TIME ZONE"), Some(Command::Show("timezone".into())));
        assert_eq!(
            parse("show session authorization"),
            Some(Command::Show("session_authorization".into()))
        );
        assert_eq!(parse("show MYAPP.USER_ID"), Some(Command::Show("myapp.user_id".into())));
        assert_eq!(parse("show tables"), None);
    }
}
