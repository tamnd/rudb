//! `COPY ... FROM STDIN` and `COPY ... TO STDOUT`, the copy sub-protocol of PostgreSQL.
//!
//! The server runs these two forms itself, because the data goes over the connection and not into
//! a file. `COPY TO STDOUT` runs the query, or a `SELECT` of the table, and sends the rows in
//! `CopyData` messages in the formats of `copyto.c`. `COPY FROM STDIN` sends `CopyInResponse`, and
//! the main loop gives each `CopyData` to [`Load`]. It reads the rows as `copyfromparse.c` reads
//! them, into one list of values for each column, and writes them through the bulk path of the
//! engine. A `COPY` of a file or of a program goes to the engine.

use std::ops::Range;
use std::time::{SystemTime, UNIX_EPOCH};

use rudb::LoadTarget;
use rudb_common::{Fields, Value};
use rudb_pgtypes::{
    DateTimeInput, InputSettings, NoZones, PgType, RowEncoder, TypeError, UNIX_TO_POSTGRES_USECS,
    ZoneAbbrevs, column_value,
};
use rudb_pgwire::{CommandTag, OutBuf};

use super::setting::{Token, loose_spanned};
use super::{FLUSH_AT, Failure, Outcome, Runner, Severity, aborted_failure, column_type, output};

/// The rows that a load holds before it writes them to the table.
const BATCH: usize = 1 << 16;

/// The size of the `CopyData` messages of `COPY TO`: one or more whole rows, up to this size.
const MESSAGE: usize = 64 << 10;

/// The length of a value that the context of an error shows, `MAX_COPY_DATA_DISPLAY`.
const DISPLAY: usize = 100;

/// The signature of the binary format, the first 11 bytes of the data.
const SIGNATURE: &[u8; 11] = b"PGCOPY\n\xff\r\n\0";

/// A `COPY` statement with `STDIN` or `STDOUT`.
#[derive(Debug, Clone)]
pub(super) struct Copy {
    source: Source,
    /// `FROM`, else `TO`. PostgreSQL takes `STDIN` and `STDOUT` in both directions.
    pub(super) from: bool,
    options: Options,
}

#[derive(Debug, Clone)]
enum Source {
    /// A table and its column list, empty for all the columns.
    Table { name: Vec<String>, columns: Vec<String> },
    /// The byte range of the query of `COPY (query) TO`.
    Query(Range<usize>),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Format {
    Text,
    Csv,
    Binary,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Header {
    /// The number of header lines, which `COPY FROM` skips.
    Lines(u64),
    Match,
}

/// What `COPY FROM` does with a value that the input function of its type does not take.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum OnError {
    Stop,
    Ignore,
    SetNull,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Verbosity {
    Silent,
    Default,
    Verbose,
}

/// The columns of `FORCE_QUOTE`, `FORCE_NOT_NULL` or `FORCE_NULL`.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Force {
    No,
    All,
    Columns(Vec<String>),
}

impl Force {
    fn given(&self) -> bool {
        *self != Force::No
    }

    /// The flag of each column of the copy, or the name of a column that is not in the copy.
    fn flags(&self, names: &[String]) -> Result<Vec<bool>, String> {
        match self {
            Force::No => Ok(vec![false; names.len()]),
            Force::All => Ok(vec![true; names.len()]),
            Force::Columns(columns) => {
                let mut flags = vec![false; names.len()];
                for column in columns {
                    let at = names.iter().position(|name| name == column).ok_or(column.clone())?;
                    flags[at] = true;
                }
                Ok(flags)
            }
        }
    }
}

/// The options of a copy after `ProcessCopyOptions`.
#[derive(Debug, Clone)]
struct Options {
    format: Format,
    delimiter: u8,
    null: Vec<u8>,
    header: Header,
    quote: u8,
    escape: u8,
    force_quote: Force,
    force_not_null: Force,
    force_null: Force,
    on_error: OnError,
    verbosity: Verbosity,
    /// The rows that `ON_ERROR IGNORE` can skip, or 0 for no limit.
    reject_limit: u64,
}

/// The argument of an option, as `copy_generic_opt_arg` reads it.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Arg {
    None,
    /// A string, a word or a number with a fraction, as written.
    Text(String),
    Int(i64),
    Star,
    List(Vec<String>),
}

/// An option with the byte offset of its name, for the position of an error.
#[derive(Debug, Clone)]
struct Item {
    name: String,
    arg: Arg,
    at: usize,
}

fn failure(sqlstate: &str, message: impl Into<String>) -> Failure {
    Failure::new(sqlstate, message.into())
}

fn placed(sqlstate: &str, message: impl Into<String>, at: usize) -> Failure {
    Failure { position: Some(at), ..failure(sqlstate, message) }
}

/// A failure with the context of the copy, the `W` field.
fn within(mut failure: Failure, context: String) -> Failure {
    let fields = failure.fields.get_or_insert_with(|| Box::new(Fields::default()));
    fields.context = Some(match fields.context.take() {
        Some(inner) => format!("{inner}\n{context}"),
        None => context,
    });
    failure
}

fn type_failure(error: TypeError) -> Failure {
    let mut failure = failure(error.sqlstate.as_str(), error.message);
    if error.detail.is_some() || error.hint.is_some() {
        let mut fields = Fields::default();
        fields.detail = error.detail;
        fields.hint = error.hint;
        failure.fields = Some(Box::new(fields));
    }
    failure
}

/// Reads a `COPY` with `STDIN` or `STDOUT`, or gives `None` for another statement and for a copy
/// of a file or a program, which the engine runs. A position in an error is a byte offset in
/// `sql`.
pub(super) fn parse(sql: &str) -> Option<Result<Copy, Failure>> {
    let head = sql.trim_start().as_bytes();
    if !head.get(..4).is_some_and(|word| word.eq_ignore_ascii_case(b"copy")) {
        return None;
    }
    let (mut tokens, mut spans) = loose_spanned(sql)?;
    while tokens.last() == Some(&Token::Punct(';')) {
        tokens.pop();
        spans.pop();
    }
    let mut p = Reader { sql, tokens: &tokens, spans: &spans, at: 0 };
    if !p.eat("copy") {
        return None;
    }
    let mut items = Vec::new();
    if p.peek_is("binary") {
        items.push(Item { name: "format".into(), arg: Arg::Text("binary".into()), at: p.place() });
        p.at += 1;
    }
    let source = if p.punct('(') {
        let open = p.at;
        let close = p.closing(open)?;
        p.at = close + 1;
        Source::Query(spans[open] + 1..spans[close])
    } else {
        let name = p.name()?;
        let columns = if p.punct('(') {
            p.at += 1;
            let columns = p.names()?;
            if !p.punct(')') {
                return None;
            }
            p.at += 1;
            columns
        } else {
            Vec::new()
        };
        Source::Table { name, columns }
    };
    let from = if p.eat("from") {
        true
    } else if p.eat("to") {
        false
    } else {
        return None;
    };
    if !(p.eat("stdin") || p.eat("stdout")) {
        return None;
    }
    // From here the statement is the server's, so what does not read is a syntax error.
    Some(p.rest(source, from, items))
}

struct Reader<'a> {
    sql: &'a str,
    tokens: &'a [Token],
    spans: &'a [usize],
    at: usize,
}

impl Reader<'_> {
    fn peek(&self) -> Option<&Token> {
        self.tokens.get(self.at)
    }

    fn peek_is(&self, word: &str) -> bool {
        self.peek().is_some_and(|token| token.is(word))
    }

    fn eat(&mut self, word: &str) -> bool {
        let found = self.peek_is(word);
        if found {
            self.at += 1;
        }
        found
    }

    fn punct(&self, c: char) -> bool {
        self.peek() == Some(&Token::Punct(c))
    }

    /// The byte offset of the next token, or the end of the statement.
    fn place(&self) -> usize {
        self.spans.get(self.at).copied().unwrap_or(self.sql.trim_end().len())
    }

    /// The `)` of the `(` at `open`.
    fn closing(&self, open: usize) -> Option<usize> {
        let mut depth = 0usize;
        for (at, token) in self.tokens.iter().enumerate().skip(open) {
            match token {
                Token::Punct('(') => depth += 1,
                Token::Punct(')') => {
                    depth -= 1;
                    if depth == 0 {
                        return Some(at);
                    }
                }
                _ => {}
            }
        }
        None
    }

    fn word(&mut self) -> Option<String> {
        match self.peek()? {
            Token::Word { text, .. } => {
                let text = text.clone();
                self.at += 1;
                Some(text)
            }
            _ => None,
        }
    }

    fn name(&mut self) -> Option<Vec<String>> {
        let mut name = vec![self.word()?];
        while self.punct('.') {
            self.at += 1;
            name.push(self.word()?);
        }
        Some(name)
    }

    fn names(&mut self) -> Option<Vec<String>> {
        let mut names = vec![self.word()?];
        while self.punct(',') {
            self.at += 1;
            names.push(self.word()?);
        }
        Some(names)
    }

    fn string(&mut self) -> Option<String> {
        match self.peek()? {
            Token::String(text) => {
                let text = text.clone();
                self.at += 1;
                Some(text)
            }
            _ => None,
        }
    }

    /// `syntax error at or near` the next token.
    fn syntax_error(&self) -> Failure {
        let at = self.place();
        let Some(token) = self.peek() else {
            return placed("42601", "syntax error at end of input", at);
        };
        let near = match token {
            Token::Word { text, quoted: false } => text.clone(),
            Token::Punct(c) => c.to_string(),
            _ => {
                let end = self.spans.get(self.at + 1).copied().unwrap_or(self.sql.len());
                self.sql[at..end].trim_end().to_owned()
            }
        };
        placed("42601", format!("syntax error at or near \"{near}\""), at)
    }

    /// The options and the end of the statement.
    fn rest(&mut self, source: Source, from: bool, mut items: Vec<Item>) -> Result<Copy, Failure> {
        if matches!(source, Source::Query(_)) && from {
            self.at -= 2;
            return Err(self.syntax_error());
        }
        if self.peek_is("using") || self.peek_is("delimiters") {
            self.eat("using");
            let at = self.place();
            if !self.eat("delimiters") {
                return Err(self.syntax_error());
            }
            let text = self.string().ok_or_else(|| self.syntax_error())?;
            items.push(Item { name: "delimiter".into(), arg: Arg::Text(text), at });
        }
        self.eat("with");
        if self.punct('(') {
            self.at += 1;
            loop {
                let at = self.place();
                let name = self.word().ok_or_else(|| self.syntax_error())?;
                let arg = self.arg()?;
                items.push(Item { name, arg, at });
                if self.punct(',') {
                    self.at += 1;
                } else if self.punct(')') {
                    self.at += 1;
                    break;
                } else {
                    return Err(self.syntax_error());
                }
            }
        } else {
            self.old_options(&mut items)?;
        }
        if self.peek_is("where") {
            return Err(failure("0A000", "COPY FROM with WHERE is not supported yet"));
        }
        if self.peek().is_some() {
            return Err(self.syntax_error());
        }
        let options = options(&items, from)?;
        Ok(Copy { source, from, options })
    }

    /// `copy_generic_opt_arg`.
    fn arg(&mut self) -> Result<Arg, Failure> {
        let arg = match self.peek() {
            Some(Token::String(text)) => Arg::Text(text.clone()),
            Some(Token::Word { text, .. }) => Arg::Text(text.clone()),
            Some(Token::Number { text, integer }) => number(text, *integer, ""),
            Some(Token::Punct('*')) => Arg::Star,
            Some(Token::Punct('+' | '-')) => {
                let sign = if self.punct('-') { "-" } else { "" };
                self.at += 1;
                match self.peek() {
                    Some(Token::Number { text, integer }) => number(text, *integer, sign),
                    _ => return Err(self.syntax_error()),
                }
            }
            Some(Token::Punct('(')) => {
                self.at += 1;
                let mut list = Vec::new();
                loop {
                    match self.peek() {
                        Some(Token::String(text) | Token::Word { text, .. }) => {
                            list.push(text.clone());
                        }
                        _ => return Err(self.syntax_error()),
                    }
                    self.at += 1;
                    if self.punct(',') {
                        self.at += 1;
                    } else if self.punct(')') {
                        break;
                    } else {
                        return Err(self.syntax_error());
                    }
                }
                Arg::List(list)
            }
            _ => return Ok(Arg::None),
        };
        self.at += 1;
        Ok(arg)
    }

    /// The options of the grammar before PostgreSQL 9.0, `copy_opt_list`.
    fn old_options(&mut self, items: &mut Vec<Item>) -> Result<(), Failure> {
        loop {
            let at = self.place();
            let mut item = |name: &str, arg: Arg| items.push(Item { name: name.into(), arg, at });
            if self.eat("binary") {
                item("format", Arg::Text("binary".into()));
            } else if self.eat("csv") {
                item("format", Arg::Text("csv".into()));
            } else if self.eat("freeze") {
                item("freeze", Arg::None);
            } else if self.eat("header") {
                item("header", Arg::None);
            } else if self.peek_is("delimiter")
                || self.peek_is("null")
                || self.peek_is("quote")
                || self.peek_is("escape")
                || self.peek_is("encoding")
            {
                let name = self.word().unwrap_or_default();
                if name != "encoding" {
                    self.eat("as");
                }
                let text = self.string().ok_or_else(|| self.syntax_error())?;
                items.push(Item { name, arg: Arg::Text(text), at });
            } else if self.eat("force") {
                let name = if self.eat("quote") {
                    "force_quote"
                } else if self.eat("not") {
                    if !self.eat("null") {
                        return Err(self.syntax_error());
                    }
                    "force_not_null"
                } else if self.eat("null") {
                    "force_null"
                } else {
                    return Err(self.syntax_error());
                };
                let arg = if self.punct('*') {
                    self.at += 1;
                    Arg::Star
                } else {
                    Arg::List(self.names().ok_or_else(|| self.syntax_error())?)
                };
                items.push(Item { name: name.into(), arg, at });
            } else {
                return Ok(());
            }
        }
    }
}

/// A number of an option: an integer that fits in 64 bits, else its text.
fn number(text: &str, integer: bool, sign: &str) -> Arg {
    let text = format!("{sign}{text}");
    match text.parse() {
        Ok(value) if integer => Arg::Int(value),
        _ => Arg::Text(text),
    }
}

/// The value of a Boolean option, as `defGetBoolean` reads it, or `None` for another value.
fn truth(arg: &Arg) -> Option<bool> {
    match arg {
        Arg::None => Some(true),
        Arg::Int(0) => Some(false),
        Arg::Int(1) => Some(true),
        Arg::Text(text) => match text.to_ascii_lowercase().as_str() {
            "true" | "on" => Some(true),
            "false" | "off" => Some(false),
            _ => None,
        },
        _ => None,
    }
}

/// `defGetBoolean`.
fn boolean(item: &Item) -> Result<bool, Failure> {
    truth(&item.arg)
        .ok_or_else(|| failure("42601", format!("{} requires a Boolean value", item.name)))
}

/// `defGetCopyHeaderOption`.
fn header_option(item: &Item, from: bool) -> Result<Header, Failure> {
    // A string that is an integer counts as the integer.
    let number = match &item.arg {
        Arg::Text(text) => text.trim().parse().ok().map(Arg::Int),
        _ => None,
    };
    match number.as_ref().unwrap_or(&item.arg) {
        Arg::Int(lines) => {
            let Ok(lines) = u64::try_from(*lines) else {
                let message = "a negative integer value cannot be specified for header";
                return Err(failure("22023", message));
            };
            if !from && lines > 1 {
                return Err(failure("0A000", "cannot use multi-line header in COPY TO"));
            }
            return Ok(Header::Lines(lines));
        }
        Arg::Text(text) if text.eq_ignore_ascii_case("match") => {
            if !from {
                return Err(failure("0A000", "cannot use \"match\" with HEADER in COPY TO"));
            }
            return Ok(Header::Match);
        }
        _ => {}
    }
    match truth(&item.arg) {
        Some(on) => Ok(Header::Lines(u64::from(on))),
        None => Err(failure(
            "42601",
            "header requires a Boolean value, an integer value greater than or equal to zero, or \
             the string \"match\"",
        )),
    }
}

/// `defGetString`.
fn string(item: &Item) -> Result<String, Failure> {
    match &item.arg {
        Arg::Text(text) => Ok(text.clone()),
        Arg::Int(value) => Ok(value.to_string()),
        Arg::Star => Ok("*".to_owned()),
        Arg::List(list) => Ok(list.join(", ")),
        Arg::None => Err(failure("42601", format!("{} requires a parameter", item.name))),
    }
}

/// The options of a copy, as `ProcessCopyOptions` checks them.
fn options(items: &[Item], from: bool) -> Result<Options, Failure> {
    let mut seen: Vec<&str> = Vec::new();
    let mut format = None;
    let (mut delimiter, mut null, mut quote, mut escape) = (None, None, None, None);
    let mut header = None;
    let (mut force_quote, mut force_not_null, mut force_null) = (Force::No, Force::No, Force::No);
    let mut freeze = false;
    let (mut on_error, mut verbosity, mut reject_limit) = (OnError::Stop, Verbosity::Default, None);
    let cannot = |what: &str, with: &str| format!("COPY {what} cannot be used with {with}");
    for item in items {
        let name = item.name.as_str();
        let redundant = || placed("42601", "conflicting or redundant options", item.at);
        let known = [
            "format",
            "freeze",
            "delimiter",
            "null",
            "default",
            "header",
            "quote",
            "escape",
            "force_quote",
            "force_not_null",
            "force_null",
            "convert_selectively",
            "encoding",
            "on_error",
            "reject_limit",
            "log_verbosity",
        ];
        if !known.contains(&name) {
            return Err(placed("42601", format!("option \"{name}\" not recognized"), item.at));
        }
        if seen.contains(&name) {
            return Err(redundant());
        }
        seen.push(name);
        match name {
            "format" => {
                let text = string(item)?;
                format = Some(match text.as_str() {
                    "text" => Format::Text,
                    "csv" => Format::Csv,
                    "binary" => Format::Binary,
                    _ => {
                        return Err(placed(
                            "22023",
                            format!("COPY format \"{text}\" not recognized"),
                            item.at,
                        ));
                    }
                });
            }
            "freeze" => freeze = boolean(item)?,
            "delimiter" => delimiter = Some(string(item)?),
            "null" => null = Some(string(item)?),
            "default" => {
                return Err(failure("0A000", "COPY DEFAULT is not supported yet"));
            }
            "header" => header = Some(header_option(item, from)?),
            "quote" => quote = Some(string(item)?),
            "escape" => escape = Some(string(item)?),
            "force_quote" | "force_not_null" | "force_null" => {
                let force = match &item.arg {
                    Arg::Star => Force::All,
                    Arg::List(list) => Force::Columns(list.clone()),
                    _ => {
                        return Err(placed(
                            "22023",
                            format!("argument to option \"{name}\" must be a list of column names"),
                            item.at,
                        ));
                    }
                };
                match name {
                    "force_quote" => force_quote = force,
                    "force_not_null" => force_not_null = force,
                    _ => force_null = force,
                }
            }
            "encoding" => {
                let text = string(item)?;
                let clean: String = text
                    .chars()
                    .filter(char::is_ascii_alphanumeric)
                    .map(|c| c.to_ascii_lowercase())
                    .collect();
                if !ENCODINGS.contains(&clean.as_str()) {
                    let message = "argument to option \"encoding\" must be a valid encoding name";
                    return Err(placed("22023", message, item.at));
                }
                if !["utf8", "unicode", "sqlascii"].contains(&clean.as_str()) {
                    let message = format!("COPY with ENCODING \"{text}\" is not supported yet");
                    return Err(failure("0A000", message));
                }
            }
            "on_error" => {
                if !from {
                    return Err(placed("22023", cannot("ON_ERROR", "COPY TO"), item.at));
                }
                let text = string(item)?;
                on_error = match text.to_ascii_lowercase().as_str() {
                    "stop" => OnError::Stop,
                    "ignore" => OnError::Ignore,
                    "set_null" => OnError::SetNull,
                    _ => {
                        let message = format!("COPY ON_ERROR \"{text}\" not recognized");
                        return Err(placed("22023", message, item.at));
                    }
                };
            }
            "log_verbosity" => {
                let text = string(item)?;
                verbosity = match text.to_ascii_lowercase().as_str() {
                    "silent" => Verbosity::Silent,
                    "default" => Verbosity::Default,
                    "verbose" => Verbosity::Verbose,
                    _ => {
                        let message = format!("COPY LOG_VERBOSITY \"{text}\" not recognized");
                        return Err(placed("22023", message, item.at));
                    }
                };
            }
            "reject_limit" => {
                let limit = match &item.arg {
                    Arg::Int(limit) => *limit,
                    _ => {
                        let message = format!("{name} requires an integer value");
                        return Err(failure("42601", message));
                    }
                };
                if limit <= 0 {
                    let message = format!("REJECT_LIMIT ({limit}) must be greater than zero");
                    return Err(failure("22023", message));
                }
                reject_limit = Some(limit as u64);
            }
            _ => {}
        }
    }
    let format = format.unwrap_or(Format::Text);
    let csv = format == Format::Csv;
    let binary_mode =
        |what: &str| failure("42601", format!("cannot specify {what} in BINARY mode"));
    if format == Format::Binary {
        if delimiter.is_some() {
            return Err(binary_mode("DELIMITER"));
        }
        if null.is_some() {
            return Err(binary_mode("NULL"));
        }
    }
    let delimiter = delimiter.unwrap_or_else(|| if csv { "," } else { "\t" }.to_owned());
    let null = null.unwrap_or_else(|| if csv { "" } else { "\\N" }.to_owned());
    if csv && quote.is_none() {
        quote = Some("\"".to_owned());
    }
    if csv && escape.is_none() {
        escape.clone_from(&quote);
    }
    if delimiter.len() != 1 {
        return Err(failure("0A000", "COPY delimiter must be a single one-byte character"));
    }
    let delim = delimiter.as_bytes()[0];
    if delim == b'\n' || delim == b'\r' {
        return Err(failure("22023", "COPY delimiter cannot be newline or carriage return"));
    }
    if null.contains(['\r', '\n']) {
        return Err(failure(
            "22023",
            "COPY null representation cannot use newline or carriage return",
        ));
    }
    if !csv && b"\\.abcdefghijklmnopqrstuvwxyz0123456789".contains(&delim) {
        return Err(failure("22023", format!("COPY delimiter cannot be \"{delimiter}\"")));
    }
    if format == Format::Binary && header.is_some_and(|header| header != Header::Lines(0)) {
        return Err(binary_mode("HEADER"));
    }
    let csv_mode = |what: &str| failure("0A000", format!("COPY {what} requires CSV mode"));
    if !csv && quote.is_some() {
        return Err(csv_mode("QUOTE"));
    }
    let quote = quote.unwrap_or_default();
    if csv && quote.len() != 1 {
        return Err(failure("0A000", "COPY quote must be a single one-byte character"));
    }
    if csv && delim == quote.as_bytes()[0] {
        return Err(failure("22023", "COPY delimiter and quote must be different"));
    }
    if !csv && escape.is_some() {
        return Err(csv_mode("ESCAPE"));
    }
    let escape = escape.unwrap_or_default();
    if csv && escape.len() != 1 {
        return Err(failure("0A000", "COPY escape must be a single one-byte character"));
    }
    if force_quote.given() {
        if !csv {
            return Err(csv_mode("FORCE_QUOTE"));
        }
        if from {
            return Err(failure("0A000", cannot("FORCE_QUOTE", "COPY FROM")));
        }
    }
    if force_not_null.given() {
        if !csv {
            return Err(csv_mode("FORCE_NOT_NULL"));
        }
        if !from {
            return Err(failure("22023", cannot("FORCE_NOT_NULL", "COPY TO")));
        }
    }
    if force_null.given() {
        if !csv {
            return Err(csv_mode("FORCE_NULL"));
        }
        if !from {
            return Err(failure("22023", cannot("FORCE_NULL", "COPY TO")));
        }
    }
    if null.as_bytes().contains(&delim) {
        return Err(failure(
            "22023",
            "COPY delimiter character must not appear in the NULL specification",
        ));
    }
    if csv && null.as_bytes().contains(&quote.as_bytes()[0]) {
        return Err(failure(
            "22023",
            "CSV quote character must not appear in the NULL specification",
        ));
    }
    if freeze && !from {
        return Err(failure("22023", cannot("FREEZE", "COPY TO")));
    }
    if format == Format::Binary && on_error != OnError::Stop {
        return Err(failure("42601", "only ON_ERROR STOP is allowed in BINARY mode"));
    }
    if reject_limit.is_some() && on_error != OnError::Ignore {
        let message = "COPY REJECT_LIMIT requires ON_ERROR to be set to IGNORE";
        return Err(failure("22023", message));
    }
    Ok(Options {
        format,
        delimiter: delim,
        null: null.into_bytes(),
        header: header.unwrap_or(Header::Lines(0)),
        quote: quote.bytes().next().unwrap_or(b'"'),
        escape: escape.bytes().next().unwrap_or(b'"'),
        force_quote,
        force_not_null,
        force_null,
        on_error,
        verbosity,
        reject_limit: reject_limit.unwrap_or(0),
    })
}

/// The names of the encodings of PostgreSQL and their aliases, as `clean_encoding_name` makes
/// them.
const ENCODINGS: &[&str] = &[
    "abc",
    "alt",
    "big5",
    "cp1250",
    "cp1251",
    "cp1252",
    "cp1253",
    "cp1254",
    "cp1255",
    "cp1256",
    "cp1257",
    "cp1258",
    "cp866",
    "cp874",
    "cp932",
    "cp936",
    "cp949",
    "cp950",
    "eucchinese",
    "euccn",
    "eucjis2004",
    "eucjp",
    "euckr",
    "euctw",
    "gb18030",
    "gbk",
    "iso88591",
    "iso885910",
    "iso885913",
    "iso885914",
    "iso885915",
    "iso885916",
    "iso88592",
    "iso88593",
    "iso88594",
    "iso88595",
    "iso88596",
    "iso88597",
    "iso88598",
    "iso88599",
    "johab",
    "koi8",
    "koi8r",
    "koi8u",
    "latin1",
    "latin10",
    "latin2",
    "latin3",
    "latin4",
    "latin5",
    "latin6",
    "latin7",
    "latin8",
    "latin9",
    "mskanji",
    "muleinternal",
    "shiftjis",
    "shiftjis2004",
    "sjis",
    "sqlascii",
    "tcvn",
    "tcvn5712",
    "uhc",
    "unicode",
    "utf8",
    "vscii",
    "win",
    "win1250",
    "win1251",
    "win1252",
    "win1253",
    "win1254",
    "win1255",
    "win1256",
    "win1257",
    "win1258",
    "win866",
    "win874",
    "win932",
    "win936",
    "win949",
    "win950",
    "windows1250",
    "windows1251",
    "windows1252",
    "windows1253",
    "windows1254",
    "windows1255",
    "windows1256",
    "windows1257",
    "windows1258",
    "windows866",
    "windows874",
    "windows932",
    "windows936",
    "windows949",
    "windows950",
];

/// A name in double quotes, for a statement that the server writes.
fn quoted(name: &str) -> String {
    format!("\"{}\"", name.replace('"', "\"\""))
}

/// The value in the context of an error, cut as `limit_printout_length` cuts it.
fn shown(bytes: &[u8]) -> String {
    let text = String::from_utf8_lossy(bytes);
    if text.len() <= DISPLAY {
        return text.into_owned();
    }
    let mut end = DISPLAY;
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}...", &text[..end])
}

/// The end of a line that the first line of the text or the CSV format sets.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Eol {
    Nl,
    Cr,
    CrNl,
}

/// A `COPY FROM STDIN` that reads data.
pub(super) struct Load {
    target: LoadTarget,
    /// The place in the table of each column of the copy.
    given: Vec<usize>,
    types: Vec<PgType>,
    names: Vec<String>,
    /// The name of the table in the context of an error.
    table: String,
    options: Options,
    force_not_null: Vec<bool>,
    force_null: Vec<bool>,
    /// The data that is not read yet.
    pending: Vec<u8>,
    /// The lines or the binary rows that were read.
    line: u64,
    eol: Option<Eol>,
    /// The end of the data came, `\.` or the trailer of the binary format.
    ended: bool,
    /// The header lines that are still to come.
    heading: u64,
    /// The header of the binary format is still to come.
    signature: bool,
    /// The rows that `ON_ERROR` skipped, or set a column of to null.
    rejected: u64,
    /// The notices of `ON_ERROR` that are not sent yet, and whether each has the context.
    notices: Vec<(String, bool)>,
    values: Vec<Vec<Value>>,
    rows: usize,
    total: u64,
    /// The rest of the `Query`, for the simple flow.
    pub(super) rest: Option<Rest>,
    /// The fields of the line that is read, as ranges in `scratch`, with `None` for a null.
    fields: Vec<Option<Range<usize>>>,
    scratch: Vec<u8>,
}

/// The statements of a `Query` after its `COPY FROM STDIN`, which run after the copy.
pub(super) struct Rest {
    pub(super) sql: String,
    pub(super) next: usize,
}

/// What reading the next line found.
enum Line {
    /// The line is `pending[..end]`, and the next one starts at `next`.
    Found { end: usize, next: usize },
    /// The end-of-copy marker, after `next` bytes.
    End,
    /// The data has no full line yet.
    More,
}

impl Load {
    fn context(&self) -> String {
        format!("COPY {}, line {}", self.table, self.line)
    }

    fn line_failure(&self, failure: Failure, line: &[u8]) -> Failure {
        let context = format!("{}: \"{}\"", self.context(), shown(line));
        within(failure, context)
    }

    /// The context of an error in the line that is read, which has no data yet.
    fn fix(&self, failure: Failure) -> Failure {
        within(failure, format!("COPY {}, line {}", self.table, self.line + 1))
    }

    /// Splits a line of the text format into `fields`, as `CopyReadAttributesText` does.
    fn text_fields(&mut self, line: &[u8]) {
        let (delim, null) = (self.options.delimiter, &self.options.null);
        self.fields.clear();
        self.scratch.clear();
        let mut i = 0;
        loop {
            let (raw, start) = (i, self.scratch.len());
            let mut found = false;
            let mut raw_end = line.len();
            while i < line.len() {
                let c = line[i];
                i += 1;
                if c == delim {
                    found = true;
                    raw_end = i - 1;
                    break;
                }
                if c != b'\\' {
                    self.scratch.push(c);
                    continue;
                }
                let Some(&c) = line.get(i) else { break };
                i += 1;
                let byte = match c {
                    b'0'..=b'7' => {
                        let mut value = u32::from(c - b'0');
                        for _ in 0..2 {
                            match line.get(i) {
                                Some(&d @ b'0'..=b'7') => {
                                    value = value * 8 + u32::from(d - b'0');
                                    i += 1;
                                }
                                _ => break,
                            }
                        }
                        (value & 0xff) as u8
                    }
                    b'x' if line.get(i).is_some_and(u8::is_ascii_hexdigit) => {
                        let mut value = hex(line[i]);
                        i += 1;
                        if let Some(&d) = line.get(i).filter(|d| d.is_ascii_hexdigit()) {
                            value = value * 16 + hex(d);
                            i += 1;
                        }
                        value
                    }
                    b'b' => 8,
                    b'f' => 12,
                    b'n' => b'\n',
                    b'r' => b'\r',
                    b't' => b'\t',
                    b'v' => 11,
                    other => other,
                };
                self.scratch.push(byte);
            }
            if &line[raw..raw_end] == null.as_slice() {
                self.scratch.truncate(start);
                self.fields.push(None);
            } else {
                self.fields.push(Some(start..self.scratch.len()));
            }
            if !found {
                break;
            }
        }
    }

    /// Splits a line of the CSV format into `fields`, as `CopyReadAttributesCSV` does.
    fn csv_fields(&mut self, line: &[u8]) -> Result<(), Failure> {
        let (delim, quote, escape) =
            (self.options.delimiter, self.options.quote, self.options.escape);
        self.fields.clear();
        self.scratch.clear();
        let mut i = 0;
        loop {
            let (raw, start) = (i, self.scratch.len());
            let mut saw_quote = false;
            let mut found = false;
            let mut raw_end;
            'field: loop {
                // Not in quotes.
                loop {
                    raw_end = i;
                    let Some(&c) = line.get(i) else { break 'field };
                    i += 1;
                    if c == delim {
                        found = true;
                        break 'field;
                    }
                    if c == quote {
                        saw_quote = true;
                        break;
                    }
                    self.scratch.push(c);
                }
                // In quotes.
                loop {
                    let Some(&c) = line.get(i) else {
                        return Err(failure("22P04", "unterminated CSV quoted field"));
                    };
                    i += 1;
                    if c == escape
                        && let Some(&next) = line.get(i)
                        && (next == escape || next == quote)
                    {
                        self.scratch.push(next);
                        i += 1;
                        continue;
                    }
                    if c == quote {
                        break;
                    }
                    self.scratch.push(c);
                }
            }
            if !saw_quote && &line[raw..raw_end] == self.options.null.as_slice() {
                self.scratch.truncate(start);
                self.fields.push(None);
            } else {
                self.fields.push(Some(start..self.scratch.len()));
            }
            if !found {
                return Ok(());
            }
        }
    }

    /// Reads the text or CSV lines in `pending`.
    fn read_lines(&mut self, settings: &InputSettings<'_>, done: bool) -> Result<(), Failure> {
        let mut pending = std::mem::take(&mut self.pending);
        let mut used = 0;
        let result = loop {
            if self.ended {
                used = pending.len();
                break Ok(());
            }
            match next_line(&pending[used..], &mut self.eol, &self.options, done) {
                Err(failure) => break Err(self.fix(failure)),
                Ok(Line::More) => break Ok(()),
                Ok(Line::End) => self.ended = true,
                Ok(Line::Found { end, next }) => {
                    self.line += 1;
                    let line = &pending[used..used + end];
                    used += next;
                    if let Err(failure) = self.read_line(line, settings) {
                        break Err(failure);
                    }
                }
            }
        };
        pending.drain(..used);
        self.pending = pending;
        result
    }

    /// Reads one line of the text or the CSV format into the values.
    fn read_line(&mut self, line: &[u8], settings: &InputSettings<'_>) -> Result<(), Failure> {
        if self.heading > 0 {
            self.heading -= 1;
            return self.check_header(line);
        }
        if self.options.format == Format::Csv {
            self.csv_fields(line).map_err(|failure| self.line_failure(failure, line))?;
        } else {
            self.text_fields(line);
        }
        let count = self.names.len();
        if self.fields.len() > count {
            let failure = failure("22P04", "extra data after last expected column");
            return Err(self.line_failure(failure, line));
        }
        let mut nulled = false;
        for column in 0..count {
            let Some(field) = self.fields.get(column).cloned() else {
                let name = &self.names[column];
                let failure = failure("22P04", format!("missing data for column \"{name}\""));
                return Err(self.line_failure(failure, line));
            };
            let mut field = field;
            if self.options.format == Format::Csv {
                if field.is_none() && self.force_not_null[column] {
                    let start = self.scratch.len();
                    self.scratch.extend_from_slice(&self.options.null);
                    field = Some(start..self.scratch.len());
                } else if let Some(range) = &field
                    && self.force_null[column]
                    && self.scratch[range.clone()] == self.options.null[..]
                {
                    field = None;
                }
            }
            let value = match field {
                None => Value::Null,
                Some(range) => {
                    let bytes = &self.scratch[range];
                    match column_value(self.types[column], false, bytes, settings) {
                        Ok(value) => value,
                        Err(error) => {
                            let (name, value) = (&self.names[column], shown(bytes));
                            let context = format!("{}, column {name}: \"{value}\"", self.context());
                            let line = self.line;
                            match self.options.on_error {
                                OnError::Stop => return Err(within(type_failure(error), context)),
                                OnError::Ignore => {
                                    self.rejected += 1;
                                    let limit = self.options.reject_limit;
                                    if limit > 0 && self.rejected > limit {
                                        let message = format!(
                                            "skipped more than REJECT_LIMIT ({limit}) rows due \
                                             to data type incompatibility"
                                        );
                                        return Err(within(failure("22P02", message), context));
                                    }
                                    if self.options.verbosity == Verbosity::Verbose {
                                        self.notices.push((
                                            format!(
                                                "skipping row due to data type incompatibility \
                                                 at line {line} for column \"{name}\": \
                                                 \"{value}\""
                                            ),
                                            true,
                                        ));
                                    }
                                    for values in &mut self.values[..column] {
                                        values.pop();
                                    }
                                    return Ok(());
                                }
                                OnError::SetNull => {
                                    if !nulled {
                                        nulled = true;
                                        self.rejected += 1;
                                    }
                                    if self.options.verbosity == Verbosity::Verbose {
                                        self.notices.push((
                                            format!(
                                                "setting to null due to data type \
                                                 incompatibility at line {line} for column \
                                                 \"{name}\": \"{value}\""
                                            ),
                                            true,
                                        ));
                                    }
                                    Value::Null
                                }
                            }
                        }
                    }
                }
            };
            self.values[column].push(value);
        }
        self.rows += 1;
        Ok(())
    }

    /// The notice at the end of a copy with `ON_ERROR`, as `CopyFrom` sends it.
    fn summary(&self) -> Option<String> {
        if self.rejected == 0 || self.options.verbosity == Verbosity::Silent {
            return None;
        }
        let n = self.rejected;
        match (self.options.on_error, n) {
            (OnError::Stop, _) => None,
            (OnError::Ignore, 1) => {
                Some("1 row was skipped due to data type incompatibility".to_owned())
            }
            (OnError::Ignore, _) => {
                Some(format!("{n} rows were skipped due to data type incompatibility"))
            }
            (OnError::SetNull, 1) => {
                Some("in 1 row, columns were set to null due to data type incompatibility".into())
            }
            (OnError::SetNull, _) => Some(format!(
                "in {n} rows, columns were set to null due to data type incompatibility"
            )),
        }
    }

    /// The header line: skipped, or for `HEADER MATCH` checked against the names of the columns.
    fn check_header(&mut self, line: &[u8]) -> Result<(), Failure> {
        if self.options.header != Header::Match {
            return Ok(());
        }
        if self.options.format == Format::Csv {
            self.csv_fields(line).map_err(|failure| self.line_failure(failure, line))?;
        } else {
            self.text_fields(line);
        }
        if self.fields.len() != self.names.len() {
            let message = format!(
                "wrong number of fields in header line: got {}, expected {}",
                self.fields.len(),
                self.names.len()
            );
            return Err(self.line_failure(failure("22P04", message), line));
        }
        for (at, field) in self.fields.iter().enumerate() {
            let expected = &self.names[at];
            let message = match field {
                None => format!(
                    "column name mismatch in header line field {}: got null value (\"{}\"), \
                     expected \"{expected}\"",
                    at + 1,
                    String::from_utf8_lossy(&self.options.null)
                ),
                Some(range) => {
                    let got = String::from_utf8_lossy(&self.scratch[range.clone()]);
                    if got == expected.as_str() {
                        continue;
                    }
                    format!(
                        "column name mismatch in header line field {}: got \"{got}\", expected \
                         \"{expected}\"",
                        at + 1
                    )
                }
            };
            return Err(self.line_failure(failure("22P04", message), line));
        }
        Ok(())
    }

    /// Reads the binary rows in `pending`.
    fn read_binary(&mut self, settings: &InputSettings<'_>, done: bool) -> Result<(), Failure> {
        let mut at = 0;
        let result = (|| {
            if self.signature {
                let Some(head) = self.pending.get(..19) else {
                    return if done && self.pending.len() < 11 {
                        Err(failure("22P04", "COPY file signature not recognized"))
                    } else if done {
                        Err(failure("22P04", "invalid COPY file header (missing length)"))
                    } else {
                        Ok(())
                    };
                };
                if &head[..11] != SIGNATURE {
                    return Err(failure("22P04", "COPY file signature not recognized"));
                }
                let flags = u32::from_be_bytes([head[11], head[12], head[13], head[14]]);
                if flags & (1 << 16) != 0 {
                    return Err(failure("22P04", "invalid COPY file header (WITH OIDS)"));
                }
                if flags & 0xfffe_0000 != 0 {
                    return Err(failure(
                        "22P04",
                        "unrecognized critical flags in COPY file header",
                    ));
                }
                let extension = u32::from_be_bytes([head[15], head[16], head[17], head[18]]);
                let end = 19 + extension as usize;
                if self.pending.len() < end {
                    return if done {
                        Err(failure("22P04", "invalid COPY file header (wrong length)"))
                    } else {
                        Ok(())
                    };
                }
                at = end;
                self.signature = false;
            }
            loop {
                if self.ended {
                    if at < self.pending.len() {
                        self.line += 1;
                        let failure = failure("22P04", "received copy data after EOF marker");
                        return Err(within(failure, self.context()));
                    }
                    return Ok(());
                }
                // PostgreSQL ends the data without an error when the field count is cut.
                let Some(count) = self.pending.get(at..at + 2) else {
                    return Ok(());
                };
                let count = i16::from_be_bytes([count[0], count[1]]);
                if count == -1 {
                    self.ended = true;
                    at += 2;
                    continue;
                }
                // The whole row first, so a row that the data cuts waits for the rest.
                let mut end = at + 2;
                let mut complete = true;
                for _ in 0..count.max(0) {
                    let Some(len) = self.pending.get(end..end + 4) else {
                        complete = false;
                        break;
                    };
                    let len = i32::from_be_bytes([len[0], len[1], len[2], len[3]]);
                    end += 4 + len.max(0) as usize;
                    if end > self.pending.len() {
                        complete = false;
                        break;
                    }
                }
                self.line += 1;
                if usize::try_from(count).ok() != Some(self.names.len()) {
                    let message =
                        format!("row field count is {count}, expected {}", self.names.len());
                    return Err(within(failure("22P04", message), self.context()));
                }
                if !complete {
                    self.line -= 1;
                    if done {
                        self.line += 1;
                        let failure = failure("22P04", "unexpected EOF in COPY data");
                        return Err(within(failure, self.context()));
                    }
                    return Ok(());
                }
                let mut place = at + 2;
                for column in 0..self.names.len() {
                    let b = &self.pending[place..place + 4];
                    let len = i32::from_be_bytes([b[0], b[1], b[2], b[3]]);
                    place += 4;
                    let value = if len == -1 {
                        Value::Null
                    } else if len < -1 {
                        let failure = failure("22P04", "invalid field size");
                        let context = format!("{}, column {}", self.context(), self.names[column]);
                        return Err(within(failure, context));
                    } else {
                        let bytes = &self.pending[place..place + len as usize];
                        place += len as usize;
                        column_value(self.types[column], true, bytes, settings).map_err(
                            |error| {
                                let context =
                                    format!("{}, column {}", self.context(), self.names[column]);
                                within(type_failure(error), context)
                            },
                        )?
                    };
                    self.values[column].push(value);
                }
                self.rows += 1;
                at = end;
            }
        })();
        self.pending.drain(..at.min(self.pending.len()));
        result
    }
}

/// The text or CSV line at the start of `buf`, as `CopyReadLineText` finds it. `done` says that
/// no more data comes. An error has no context yet.
fn next_line(
    buf: &[u8],
    eol: &mut Option<Eol>,
    options: &Options,
    done: bool,
) -> Result<Line, Failure> {
    let csv = options.format == Format::Csv;
    let (quote, escape) = (options.quote, options.escape);
    let literal = |what: &str, letter: &str| {
        if csv {
            failure("22P04", format!("unquoted {what} found in data"))
                .hint(format!("Use quoted CSV field to represent {what}."))
        } else {
            failure("22P04", format!("literal {what} found in data"))
                .hint(format!("Use \"\\{letter}\" to represent {what}."))
        }
    };
    let not_alone = || failure("22P04", "end-of-copy marker is not alone on its line");
    let mut i = 0;
    let mut in_quote = false;
    let mut last_was_escape = false;
    while i < buf.len() {
        let c = buf[i];
        if csv {
            // When the quote is the escape too, it only turns the quoted state on and off.
            if quote == escape {
                in_quote ^= c == quote;
            } else {
                if in_quote && c == escape {
                    last_was_escape = !last_was_escape;
                }
                if c == quote && !last_was_escape {
                    in_quote = !in_quote;
                }
                if c != escape {
                    last_was_escape = false;
                }
            }
            if in_quote {
                i += 1;
                continue;
            }
        } else if c == b'\\' {
            let Some(&next) = buf.get(i + 1) else {
                if done {
                    i += 1;
                    continue;
                }
                return Ok(Line::More);
            };
            if next != b'.' {
                i += 2;
                continue;
            }
            // `\.` ends the data when it is alone on its line.
            let after = buf.get(i + 2).copied();
            if after.is_none() && !done {
                return Ok(Line::More);
            }
            if after.is_some_and(|after| after != b'\n' && after != b'\r') || i > 0 {
                return Err(not_alone());
            }
            let other = matches!(
                (after, *eol),
                (Some(b'\n'), Some(Eol::Cr)) | (Some(b'\r'), Some(Eol::Nl))
            );
            if other {
                let message = "end-of-copy marker does not match previous newline style";
                return Err(failure("22P04", message));
            }
            return Ok(Line::End);
        }
        match c {
            b'\r' => match *eol {
                None | Some(Eol::CrNl) => {
                    let Some(&next) = buf.get(i + 1) else {
                        if !done {
                            return Ok(Line::More);
                        }
                        if *eol == Some(Eol::CrNl) {
                            return Err(literal("carriage return", "r"));
                        }
                        *eol = Some(Eol::Cr);
                        return Ok(Line::Found { end: i, next: i + 1 });
                    };
                    if next == b'\n' {
                        *eol = Some(Eol::CrNl);
                        return Ok(Line::Found { end: i, next: i + 2 });
                    }
                    if *eol == Some(Eol::CrNl) {
                        return Err(literal("carriage return", "r"));
                    }
                    *eol = Some(Eol::Cr);
                    return Ok(Line::Found { end: i, next: i + 1 });
                }
                Some(Eol::Nl) => return Err(literal("carriage return", "r")),
                Some(Eol::Cr) => return Ok(Line::Found { end: i, next: i + 1 }),
            },
            b'\n' => match *eol {
                None | Some(Eol::Nl) => {
                    *eol = Some(Eol::Nl);
                    return Ok(Line::Found { end: i, next: i + 1 });
                }
                _ => return Err(literal("newline", "n")),
            },
            _ => i += 1,
        }
    }
    if done && !buf.is_empty() {
        return Ok(Line::Found { end: buf.len(), next: buf.len() });
    }
    Ok(Line::More)
}

fn hex(digit: u8) -> u8 {
    match digit {
        b'0'..=b'9' => digit - b'0',
        b'a'..=b'f' => digit - b'a' + 10,
        _ => digit - b'A' + 10,
    }
}

impl Failure {
    fn hint(mut self, hint: String) -> Failure {
        self.fields.get_or_insert_with(|| Box::new(Fields::default())).hint = Some(hint);
        self
    }
}

impl Runner {
    /// The settings of the input functions, as `Bind` has them.
    fn input(&self) -> InputSettings<'_> {
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_or(0, |since| i64::try_from(since.as_micros()).unwrap_or(0));
        InputSettings {
            datetime: DateTimeInput {
                order: self.format.date_format.order,
                zone: &self.zone,
                zones: &NoZones,
                abbrevs: ZoneAbbrevs::postgres_default(),
                now: now + UNIX_TO_POSTGRES_USECS,
            },
            interval_style: self.format.interval_style,
        }
    }

    /// The table of a copy, with the PostgreSQL error for a name that is not a table.
    fn copy_table(&self, name: &[String], from: bool) -> Result<LoadTarget, Failure> {
        let parts: Vec<&str> = name.iter().map(String::as_str).collect();
        self.connection.load_target(&parts).map_err(|_| {
            let written = name.join(".");
            let sql = format!("SELECT * FROM {} LIMIT 0", qualified(name));
            if self.connection.query(&sql).is_ok() {
                let (word, hint) = if from {
                    ("to", "To enable copying to a view, provide an INSTEAD OF INSERT trigger.")
                } else {
                    ("from", "Try the COPY (SELECT ...) TO variant.")
                };
                failure("42809", format!("cannot copy {word} view \"{}\"", name[name.len() - 1]))
                    .hint(hint.to_owned())
            } else {
                failure("42P01", format!("relation \"{written}\" does not exist"))
            }
        })
    }

    /// The columns of a copy of a table: their places in the table and their names.
    fn copy_columns(
        target: &LoadTarget,
        columns: &[String],
    ) -> Result<(Vec<usize>, Vec<String>), Failure> {
        let fields = target.columns();
        if columns.is_empty() {
            let names = fields.iter().map(|field| field.name.clone()).collect();
            return Ok(((0..fields.len()).collect(), names));
        }
        let mut given = Vec::with_capacity(columns.len());
        for column in columns {
            let Some(at) = fields.iter().position(|field| field.name.eq_ignore_ascii_case(column))
            else {
                return Err(failure(
                    "42703",
                    format!(
                        "column \"{column}\" of relation \"{}\" does not exist",
                        target.table()
                    ),
                ));
            };
            if given.contains(&at) {
                return Err(failure(
                    "42701",
                    format!("column \"{column}\" specified more than once"),
                ));
            }
            given.push(at);
        }
        let names = given.iter().map(|&at| fields[at].name.clone()).collect();
        Ok((given, names))
    }

    /// Runs `COPY ... TO STDOUT`: the `CopyOutResponse`, the rows in `CopyData` messages and the
    /// `CopyDone`. Gives the number of rows, and the caller sends the `CommandComplete`.
    /// `offset` is the place of the statement in `sql`.
    pub(super) fn copy_to(
        &mut self,
        copy: &Copy,
        sql: &str,
        offset: usize,
        out: &mut OutBuf,
        flush: &mut impl FnMut(&mut OutBuf) -> std::io::Result<()>,
    ) -> std::io::Result<Result<u64, Failure>> {
        let options = &copy.options;
        let (query, at, table) = match &copy.source {
            Source::Query(range) => {
                let range = offset + range.start..offset + range.end;
                (sql[range.clone()].to_owned(), range.start, None)
            }
            Source::Table { name, columns } => {
                let target = match self.copy_table(name, false) {
                    Ok(target) => target,
                    Err(failure) => return Ok(Err(failure)),
                };
                let (given, names) = match Runner::copy_columns(&target, columns) {
                    Ok(found) => found,
                    Err(failure) => return Ok(Err(failure)),
                };
                let fields = target.columns();
                let list: Vec<String> = given.iter().map(|&at| quoted(&fields[at].name)).collect();
                let table = format!("{}.{}", quoted(target.schema()), quoted(target.table()));
                let all = fields.iter().map(|field| field.name.clone()).collect();
                let query = format!("SELECT {} FROM {table}", list.join(", "));
                (query, offset, Some((names, all, target.table().to_owned())))
            }
        };
        let ran = self.run(None, None, sql, at, out, |c| c.execute(&query));
        let result = match ran {
            Ok(Outcome::Result(result)) => result,
            Ok(Outcome::Done(_)) => return Ok(Ok(0)),
            Err(failure) => return Ok(Err(failure)),
        };
        let (names, all, table) = match table {
            Some((names, all, table)) => (names, all, Some(table)),
            None => (result.names().to_vec(), result.names().to_vec(), None),
        };
        let force_quote = force_flags(&options.force_quote, "FORCE_QUOTE", &names, &all, &table);
        let force_quote = match force_quote {
            Ok(flags) => flags,
            Err(failure) => return Ok(Err(failure)),
        };
        let binary = options.format == Format::Binary;
        let columns: Vec<_> = result
            .types()
            .iter()
            .enumerate()
            .map(|(at, ty)| (ty.clone(), column_type(ty, result.origin(at)).oid, binary))
            .collect();
        let mut encoder = match RowEncoder::new(&columns) {
            Ok(encoder) => encoder,
            Err(error) => return Ok(Err(type_failure(error))),
        };
        let width = columns.len();
        let code = i16::from(binary);
        out.copy_out_response(i8::from(binary), &vec![code; width]);
        let mut data = Vec::with_capacity(MESSAGE);
        if binary {
            data.extend_from_slice(SIGNATURE);
            data.extend_from_slice(&[0; 8]);
        } else if options.header == Header::Lines(1) {
            for (at, name) in names.iter().enumerate() {
                if at > 0 {
                    data.push(options.delimiter);
                }
                if options.format == Format::Csv {
                    csv_value(options, name.as_bytes(), false, width == 1, &mut data);
                } else {
                    text_value(options.delimiter, name.as_bytes(), &mut data);
                }
            }
            data.push(b'\n');
        }
        let mut rows = 0u64;
        let mut scratch = Vec::new();
        for chunk in result.chunks() {
            let chunk = match chunk.clone().settled() {
                Ok(chunk) => chunk,
                Err(error) => return Ok(Err(Failure::engine(&error, 0))),
            };
            let n = chunk.len();
            scratch.clear();
            let settings = output(&self.format, &self.zone);
            if let Err(error) = encoder.encode(chunk.columns(), 0..n, &settings, &mut scratch) {
                return Ok(Err(type_failure(error)));
            }
            let mut at = 0;
            while at < scratch.len() {
                let len = u32::from_be_bytes([
                    scratch[at + 1],
                    scratch[at + 2],
                    scratch[at + 3],
                    scratch[at + 4],
                ]) as usize;
                let body = &scratch[at + 5..at + 1 + len];
                at += 1 + len;
                if binary {
                    data.extend_from_slice(body);
                } else {
                    copy_row(options, body, &force_quote, &mut data);
                }
                if data.len() >= MESSAGE {
                    out.copy_data(&data);
                    data.clear();
                    if out.len() >= FLUSH_AT {
                        flush(out)?;
                    }
                }
            }
            rows += n as u64;
        }
        if binary {
            data.extend_from_slice(&(-1i16).to_be_bytes());
        }
        if !data.is_empty() {
            out.copy_data(&data);
        }
        out.copy_done();
        Ok(Ok(rows))
    }

    /// Starts `COPY ... FROM STDIN`: checks the table and the columns and sends the
    /// `CopyInResponse`. The main loop then gives the data to [`Runner::copy_data`].
    pub(super) fn copy_from(
        &mut self,
        copy: Copy,
        rest: Option<Rest>,
        out: &mut OutBuf,
    ) -> Result<(), Failure> {
        let Source::Table { name, columns } = &copy.source else {
            return Err(failure("42601", "COPY FROM needs a table"));
        };
        let target = self.copy_table(name, true)?;
        let (given, names) = Runner::copy_columns(&target, columns)?;
        let fields = target.columns();
        // The declared types of the columns, with their typmods, are the types of the columns of
        // a query of the table.
        let list: Vec<String> = given.iter().map(|&at| quoted(&fields[at].name)).collect();
        let table = format!("{}.{}", quoted(target.schema()), quoted(target.table()));
        let sql = format!("SELECT {} FROM {table} LIMIT 0", list.join(", "));
        let shape = self.connection.query(&sql).map_err(|e| Failure::engine(&e, 0))?;
        let types: Vec<PgType> = shape
            .types()
            .iter()
            .enumerate()
            .map(|(at, ty)| column_type(ty, shape.origin(at)))
            .collect();
        let options = copy.options;
        let all: Vec<String> = fields.iter().map(|field| field.name.clone()).collect();
        let table = Some(target.table().to_owned());
        let force_not_null =
            force_flags(&options.force_not_null, "FORCE_NOT_NULL", &names, &all, &table)?;
        let force_null = force_flags(&options.force_null, "FORCE_NULL", &names, &all, &table)?;
        let binary = options.format == Format::Binary;
        let code = i16::from(binary);
        out.copy_in_response(i8::from(binary), &vec![code; names.len()]);
        let heading = match options.header {
            Header::Lines(lines) => lines,
            Header::Match => 1,
        };
        let count = names.len();
        self.load = Some(Box::new(Load {
            table: target.table().to_owned(),
            target,
            given,
            types,
            names,
            options,
            force_not_null,
            force_null,
            pending: Vec::new(),
            line: 0,
            eol: None,
            ended: false,
            heading,
            signature: binary,
            rejected: 0,
            notices: Vec::new(),
            values: vec![Vec::new(); count],
            rows: 0,
            total: 0,
            rest,
            fields: Vec::new(),
            scratch: Vec::new(),
        }));
        Ok(())
    }

    /// Reads one `CopyData` of `COPY FROM STDIN`. An error ends the copy.
    pub(super) fn copy_data(&mut self, data: &[u8], out: &mut OutBuf) -> Result<(), Failure> {
        let Some(mut load) = self.load.take() else { return Ok(()) };
        let read = self.copy_read(&mut load, data, false);
        self.copy_notices(&mut load, out);
        if read.is_ok() {
            self.load = Some(load);
        }
        read
    }

    fn copy_read(&self, load: &mut Load, data: &[u8], done: bool) -> Result<(), Failure> {
        // The text formats ignore the data after the end-of-copy marker.
        if !load.ended || load.options.format == Format::Binary {
            load.pending.extend_from_slice(data);
            let settings = self.input();
            if load.options.format == Format::Binary {
                load.read_binary(&settings, done)?;
            } else {
                load.read_lines(&settings, done)?;
            }
        }
        if load.rows >= BATCH || (done && load.rows > 0) {
            self.copy_write(load)?;
        }
        Ok(())
    }

    /// Writes the rows that the load holds to the table.
    fn copy_write(&self, load: &mut Load) -> Result<(), Failure> {
        let count = load.names.len();
        let values = std::mem::replace(&mut load.values, vec![Vec::new(); count]);
        let rows = std::mem::take(&mut load.rows);
        self.connection
            .load(&load.target, &load.given, values, rows)
            .map_err(|error| Failure { position: None, ..Failure::engine(&error, 0) })?;
        load.total += rows as u64;
        Ok(())
    }

    /// Ends `COPY FROM STDIN` at `CopyDone`: reads the last line and writes the rows. Gives the
    /// number of rows and the rest of the `Query`.
    pub(super) fn copy_done(&mut self, out: &mut OutBuf) -> Result<(u64, Option<Rest>), Failure> {
        let Some(mut load) = self.load.take() else { return Ok((0, None)) };
        let read = self.copy_read(&mut load, &[], true);
        self.copy_notices(&mut load, out);
        read?;
        if let Some(summary) = load.summary() {
            load.notices.push((summary, false));
            self.copy_notices(&mut load, out);
        }
        Ok((load.total, load.rest.take()))
    }

    /// Sends the notices of `ON_ERROR`, if `client_min_messages` lets the client have them.
    fn copy_notices(&self, load: &mut Load, out: &mut OutBuf) {
        let context = format!("COPY {}", load.table);
        for (message, placed) in load.notices.drain(..) {
            if Severity::Notice < self.least {
                continue;
            }
            let mut fields: Vec<(u8, &[u8])> = vec![
                (b'S', b"NOTICE"),
                (b'V', b"NOTICE"),
                (b'C', b"00000"),
                (b'M', message.as_bytes()),
            ];
            if placed {
                fields.push((b'W', context.as_bytes()));
            }
            out.notice_response(&fields);
        }
    }

    /// The context of an error of the protocol in `COPY FROM STDIN`, as for `CopyFail`.
    pub(super) fn copy_context(&self) -> Option<String> {
        let load = self.load.as_ref()?;
        Some(format!("COPY {}, line {}", load.table, load.line + 1))
    }

    /// Ends `COPY FROM STDIN` at `CopyFail`, with the error of PostgreSQL.
    pub(super) fn copy_fail(&mut self, text: &[u8]) -> Failure {
        let message = format!("COPY from stdin failed: {}", String::from_utf8_lossy(text));
        let failure = failure("57014", message);
        match self.load.take() {
            Some(load) => {
                let context = format!("COPY {}, line {}", load.table, load.line + 1);
                within(failure, context)
            }
            None => failure,
        }
    }

    /// Runs a `COPY` with `STDIN` or `STDOUT` that starts at byte `offset` of `sql`. For `COPY
    /// TO` it sends the rows and the `CommandComplete`. For `COPY FROM` it sends the
    /// `CopyInResponse` and gives with [`Runner::load`] set, and the main loop reads the data.
    /// `rest` is the rest of a `Query`, or `None` on the extended flow.
    pub(super) fn copy(
        &mut self,
        copy: Copy,
        sql: &str,
        offset: usize,
        rest: Option<Rest>,
        out: &mut OutBuf,
        flush: &mut impl FnMut(&mut OutBuf) -> std::io::Result<()>,
    ) -> std::io::Result<Result<(), Failure>> {
        if self.connection.transaction() == rudb::Transaction::Aborted {
            return Ok(Err(aborted_failure()));
        }
        let done = if copy.from {
            // The rows of the copy go in one transaction, so an error writes none of them.
            self.begin_implicit().and_then(|()| self.copy_from(copy, rest, out))
        } else {
            match self.copy_to(&copy, sql, offset, out, flush)? {
                Ok(rows) => {
                    out.command_tag(CommandTag::Copy, rows);
                    Ok(())
                }
                Err(failure) => Err(failure),
            }
        };
        if done.is_err() {
            self.connection.abort_transaction();
        }
        Ok(done)
    }
}

/// The flag of each column of the copy for a force option, with the error of `CopyGetAttnums`
/// for a column that is not in `all`, the columns of the table or of the query, and the error of
/// `BeginCopy` for one that is not in the copy.
fn force_flags(
    force: &Force,
    what: &str,
    names: &[String],
    all: &[String],
    table: &Option<String>,
) -> Result<Vec<bool>, Failure> {
    force.flags(names).map_err(|column| {
        if all.contains(&column) {
            failure("42P10", format!("{what} column \"{column}\" not referenced by COPY"))
        } else if let Some(table) = table {
            failure("42703", format!("column \"{column}\" of relation \"{table}\" does not exist"))
        } else {
            failure("42703", format!("column \"{column}\" does not exist"))
        }
    })
}

/// A table name for a statement that the server writes.
fn qualified(name: &[String]) -> String {
    name.iter().map(|part| quoted(part)).collect::<Vec<_>>().join(".")
}

/// One row of `COPY TO` in the text or the CSV format, from the body of a `DataRow` in the text
/// format.
fn copy_row(options: &Options, body: &[u8], force_quote: &[bool], data: &mut Vec<u8>) {
    let count = u16::from_be_bytes([body[0], body[1]]) as usize;
    let mut at = 2;
    for (column, &force) in force_quote.iter().enumerate().take(count) {
        if column > 0 {
            data.push(options.delimiter);
        }
        let len = i32::from_be_bytes([body[at], body[at + 1], body[at + 2], body[at + 3]]);
        at += 4;
        if len < 0 {
            data.extend_from_slice(&options.null);
            continue;
        }
        let value = &body[at..at + len as usize];
        at += len as usize;
        if options.format == Format::Csv {
            csv_value(options, value, force, count == 1, data);
        } else {
            text_value(options.delimiter, value, data);
        }
    }
    data.push(b'\n');
}

/// `CopyAttributeOutText`.
fn text_value(delimiter: u8, value: &[u8], data: &mut Vec<u8>) {
    for &c in value {
        let letter = match c {
            8 => b'b',
            12 => b'f',
            b'\n' => b'n',
            b'\r' => b'r',
            b'\t' => b't',
            11 => b'v',
            b'\\' => b'\\',
            c if c == delimiter => c,
            c => {
                data.push(c);
                continue;
            }
        };
        data.push(b'\\');
        data.push(letter);
    }
}

/// `CopyAttributeOutCSV`.
fn csv_value(options: &Options, value: &[u8], force: bool, single: bool, data: &mut Vec<u8>) {
    let (delim, quote, escape) = (options.delimiter, options.quote, options.escape);
    let quoting = force
        || value == options.null.as_slice()
        || (single && value == b"\\.")
        || value.iter().any(|&c| c == delim || c == quote || c == b'\n' || c == b'\r');
    if !quoting {
        data.extend_from_slice(value);
        return;
    }
    data.push(quote);
    for &c in value {
        if c == quote || c == escape {
            data.push(escape);
        }
        data.push(c);
    }
    data.push(quote);
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parsed(sql: &str) -> Copy {
        match parse(sql).expect("a copy") {
            Ok(copy) => copy,
            Err(failure) => panic!("{}", failure.message),
        }
    }

    fn error(sql: &str) -> (String, String, Option<usize>) {
        let failure = parse(sql).expect("a copy").expect_err("an error");
        (failure.sqlstate, failure.message, failure.position)
    }

    #[test]
    fn statements_that_are_not_stdin_or_stdout_are_not_copies() {
        assert!(parse("select 1").is_none());
        assert!(parse("copy t from '/tmp/x'").is_none());
        assert!(parse("copy t to program 'cat'").is_none());
    }

    #[test]
    fn the_new_and_the_old_option_syntax_read() {
        let copy = parsed("copy s.t (a, b) from stdin (format csv, header match, delimiter ';')");
        assert!(copy.from);
        let Source::Table { name, columns } = &copy.source else { panic!("a table") };
        assert_eq!(name, &["s", "t"]);
        assert_eq!(columns, &["a", "b"]);
        assert_eq!(copy.options.format, Format::Csv);
        assert_eq!(copy.options.header, Header::Match);
        assert_eq!(copy.options.delimiter, b';');

        let copy = parsed("COPY t TO STDOUT WITH CSV HEADER QUOTE AS '''' FORCE QUOTE *;");
        assert!(!copy.from);
        assert_eq!(copy.options.header, Header::Lines(1));
        assert_eq!(copy.options.quote, b'\'');
        assert_eq!(copy.options.force_quote, Force::All);

        let copy = parsed("copy binary t from stdin");
        assert_eq!(copy.options.format, Format::Binary);

        let copy = parsed("copy (select 1) to stdout");
        assert!(matches!(copy.source, Source::Query(ref range) if range.clone() == (6..14)));
    }

    #[test]
    fn header_takes_a_boolean_a_count_or_match() {
        assert_eq!(parsed("copy t from stdin (header 3)").options.header, Header::Lines(3));
        assert_eq!(parsed("copy t from stdin (header ' 2')").options.header, Header::Lines(2));
        assert_eq!(parsed("copy t from stdin (header off)").options.header, Header::Lines(0));
        assert_eq!(error("copy t to stdout (header 2)").0, "0A000");
        assert_eq!(error("copy t from stdin (header -1)").0, "22023");
        assert_eq!(error("copy t from stdin (header 'yes')").0, "42601");
        let (state, message, position) = error("copy t to stdout (header match)");
        assert_eq!(state, "0A000");
        assert_eq!(message, "cannot use \"match\" with HEADER in COPY TO");
        assert_eq!(position, None);
    }

    #[test]
    fn option_errors_have_the_codes_of_postgres() {
        assert_eq!(error("copy t to stdout (on_error ignore)").0, "22023");
        assert_eq!(
            error("copy t from stdin (on_error bad)").1,
            "COPY ON_ERROR \"bad\" not recognized"
        );
        assert_eq!(error("copy t from stdin (reject_limit 0)").0, "22023");
        assert_eq!(error("copy t from stdin (reject_limit 2)").0, "22023");
        assert_eq!(error("copy t from stdin (format binary, on_error ignore)").0, "42601");
        assert_eq!(error("copy t from stdin (freeze maybe)").1, "freeze requires a Boolean value");
        assert_eq!(error("copy t to stdout (format csv, force_not_null *)").0, "22023");
        assert_eq!(error("copy t from stdin (format csv, force_quote *)").0, "0A000");
        assert_eq!(error("copy t from stdin (format csv, format text)").0, "42601");
        assert_eq!(error("copy t from stdin (bogus)").0, "42601");
    }

    fn text() -> Options {
        parsed("copy t from stdin").options
    }

    fn csv() -> Options {
        parsed("copy t from stdin (format csv)").options
    }

    fn found(buf: &[u8], options: &Options, done: bool) -> Option<(usize, usize)> {
        let mut eol = None;
        match next_line(buf, &mut eol, options, done).ok()? {
            Line::Found { end, next } => Some((end, next)),
            Line::End | Line::More => None,
        }
    }

    #[test]
    fn the_line_reader_finds_the_end_of_a_line() {
        assert_eq!(found(b"1\tx\n2", &text(), false), Some((3, 4)));
        assert_eq!(found(b"1\tx", &text(), false), None);
        assert_eq!(found(b"1\tx", &text(), true), Some((3, 3)));
        assert_eq!(found(b"1\r\n2", &text(), false), Some((1, 3)));
        // A newline in quotes is data in CSV.
        assert_eq!(found(b"\"a\nb\",1\n", &csv(), false), Some((7, 8)));
        assert!(matches!(next_line(b"\\.\n", &mut None, &text(), false), Ok(Line::End)));
    }

    #[test]
    fn a_line_with_another_end_is_an_error() {
        let mut eol = None;
        let first = next_line(b"1\n2\r\n", &mut eol, &text(), false);
        assert!(matches!(first, Ok(Line::Found { end: 1, next: 2 })));
        let failure = next_line(b"2\r\n", &mut eol, &text(), false).err().expect("an error");
        assert_eq!(failure.sqlstate, "22P04");
        assert_eq!(failure.message, "literal carriage return found in data");
    }

    #[test]
    fn values_are_escaped_as_postgres_does() {
        let mut data = Vec::new();
        text_value(b'\t', b"a\tb\\c\nd", &mut data);
        assert_eq!(data, b"a\\tb\\\\c\\nd");

        let options = csv();
        let mut data = Vec::new();
        csv_value(&options, b"a,\"b\"", false, false, &mut data);
        assert_eq!(data, b"\"a,\"\"b\"\"\"");
        data.clear();
        csv_value(&options, b"\\.", false, true, &mut data);
        assert_eq!(data, b"\"\\.\"");
        data.clear();
        csv_value(&options, b"", false, false, &mut data);
        assert_eq!(data, b"\"\"");
    }
}
