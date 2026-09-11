//! The shell itself: read a line, decide whether it is SQL or a dot command, run it, print it.

use std::fmt::Write as _;
use std::fs::File;
use std::io::{self, BufWriter, Write};
use std::path::{Path, PathBuf};
use std::time::Instant;

use rudb::{Connection, Database, Error, QueryResult, Span};

use crate::args::{Command, Options};
use crate::format::{Format, Settings, escaped, render};

/// Where printed results go.
///
/// `.output FILE` and `.output` back again is the reason this is a type rather than a
/// `Box<dyn Write>` handed in once. A shell that can only write to the stream it was started with
/// cannot be used to produce a file, which is most of what the CSV and JSON modes are for.
enum Sink {
    /// The stream the shell was started with.
    Given(Box<dyn Write>),
    /// A file opened by `.output`.
    File(BufWriter<File>, PathBuf),
}

impl Write for Sink {
    fn write(&mut self, buffer: &[u8]) -> io::Result<usize> {
        match self {
            Self::Given(out) => out.write(buffer),
            Self::File(out, _) => out.write(buffer),
        }
    }

    fn flush(&mut self) -> io::Result<()> {
        match self {
            Self::Given(out) => out.flush(),
            Self::File(out, _) => out.flush(),
        }
    }
}

/// Why the shell stopped.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Stop {
    /// `.quit`, `.exit`, or the end of the input.
    Done,
    /// An error, and `-bail` was on.
    Failed,
}

/// A running shell.
pub struct Shell {
    database: Database,
    /// The connection statements run on, which is the thing an interrupt would have to reach.
    ///
    /// Held beside the database rather than instead of it, because `.tables` and `.schema` read the
    /// catalog and that is a database call. Replaced whenever `.open` replaces the database, so the
    /// two never name different things.
    connection: Connection,
    settings: Settings,
    out: Sink,
    err: Box<dyn Write>,
    filename: String,
    given: Option<Box<dyn Write>>,
    timer: bool,
    echo: bool,
    bail: bool,
    failed: bool,
}

impl std::fmt::Debug for Shell {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Shell")
            .field("settings", &self.settings)
            .field("failed", &self.failed)
            .finish_non_exhaustive()
    }
}

impl Shell {
    /// A shell over `database`, writing results to `out` and errors to `err`.
    pub fn new(
        options: &Options,
        database: Database,
        out: Box<dyn Write>,
        err: Box<dyn Write>,
    ) -> Self {
        Self {
            connection: database.connect(),
            database,
            settings: options.settings.clone(),
            out: Sink::Given(out),
            err,
            filename: options.database.clone(),
            given: None,
            timer: false,
            echo: options.echo,
            bail: options.bail,
            failed: false,
        }
    }

    /// Whether anything has failed since the shell started, which is what the exit code is.
    pub fn failed(&self) -> bool {
        self.failed
    }

    /// Runs everything the command line asked for, in order.
    pub fn run_commands(&mut self, commands: &[Command]) -> Stop {
        for command in commands {
            let stop = match command {
                Command::Sql(sql) => self.run_input(sql),
                Command::File(path) => self.run_file(path),
            };
            if stop == Stop::Failed {
                return Stop::Failed;
            }
        }
        Stop::Done
    }

    /// Runs a file of SQL and dot commands.
    pub fn run_file(&mut self, path: &Path) -> Stop {
        match std::fs::read_to_string(path) {
            Ok(text) => self.run_input(&text),
            Err(problem) => {
                let why = format!("Cannot open file \"{}\": {problem}", path.display());
                self.report(&Error::io(why), "");
                self.after_error()
            }
        }
    }

    /// The two lines a terminal gets before the first prompt.
    pub fn greet(&mut self) {
        let _ = writeln!(self.out, "rudb {}", crate::VERSION);
        let _ = writeln!(self.out, "Enter \".help\" for usage hints.");
        let _ = self.out.flush();
    }

    /// Reads and runs lines from a terminal until the user stops.
    ///
    /// The continuation marker is what says the statement is not finished, and it is the reason
    /// [`rudb::is_complete`] exists rather than the shell guessing from a trailing semicolon.
    pub fn prompt(&mut self, stdin: &io::Stdin) -> Stop {
        let mut pending = String::new();
        loop {
            let marker = if pending.is_empty() { "D " } else { "· " };
            let _ = write!(self.out, "{marker}");
            let _ = self.out.flush();
            let mut line = String::new();
            match stdin.read_line(&mut line) {
                Ok(0) => {
                    let _ = writeln!(self.out);
                    return Stop::Done;
                }
                Ok(_) => {}
                Err(_) => return Stop::Done,
            }
            let line = line.trim_end_matches(['\n', '\r']);
            if pending.is_empty() && line.trim_start().starts_with('.') {
                if self.run_dot(line.trim()) == Stop::Failed {
                    return Stop::Done;
                }
                continue;
            }
            if !pending.is_empty() {
                pending.push('\n');
            }
            pending.push_str(line);
            if rudb::is_complete(&pending) {
                let statement = std::mem::take(&mut pending);
                if self.run_sql(&statement) == Stop::Failed {
                    return Stop::Done;
                }
            }
        }
    }

    /// Runs a block of input, which may hold any mixture of dot commands and statements.
    ///
    /// Line oriented rather than statement oriented, because a dot command is a line and SQL is
    /// not. Lines accumulate into a statement until the tokenizer says the statement is finished,
    /// which is how a multi line `CREATE TABLE` works at a prompt and in a file alike.
    pub fn run_input(&mut self, text: &str) -> Stop {
        let mut pending = String::new();
        for line in text.lines() {
            if pending.trim().is_empty() && line.trim_start().starts_with('.') {
                pending.clear();
                if self.run_dot(line.trim()) == Stop::Failed {
                    return Stop::Failed;
                }
                continue;
            }
            if !pending.is_empty() {
                pending.push('\n');
            }
            pending.push_str(line);
            if rudb::is_complete(&pending) {
                let statement = std::mem::take(&mut pending);
                if self.run_sql(&statement) == Stop::Failed {
                    return Stop::Failed;
                }
            }
        }
        if pending.trim().is_empty() {
            return Stop::Done;
        }
        self.run_sql(&pending)
    }

    /// Runs whatever statements are in one piece of text.
    fn run_sql(&mut self, text: &str) -> Stop {
        let found = match rudb::statements(text) {
            Ok(found) => found,
            Err(problem) => {
                self.report(&problem, text);
                return self.after_error();
            }
        };
        for statement in found {
            if self.echo {
                let _ = writeln!(self.out, "{}", statement.sql());
            }
            let started = Instant::now();
            match self.connection.execute(statement.sql()) {
                Ok(result) => {
                    self.print(&result);
                    if self.timer {
                        let _ = writeln!(
                            self.err,
                            "Run Time (s): real {:.3}",
                            started.elapsed().as_secs_f64()
                        );
                    }
                }
                Err(problem) => {
                    self.report(&problem, statement.sql());
                    return self.after_error();
                }
            }
        }
        Stop::Done
    }

    /// Prints a result, unless it is the empty one a writing statement hands back.
    fn print(&mut self, result: &QueryResult) {
        let text = render(result, &self.settings);
        if !text.is_empty() {
            let _ = write!(self.out, "{text}");
            let _ = self.out.flush();
        }
    }

    /// What an error does to the run, which depends on `-bail`.
    fn after_error(&mut self) -> Stop {
        self.failed = true;
        if self.bail { Stop::Failed } else { Stop::Done }
    }

    /// Prints an error the way DuckDB prints one: the message, then the line it is about with a
    /// caret under the offending token.
    fn report(&mut self, problem: &Error, sql: &str) {
        let _ = writeln!(self.err, "{problem}");
        if let Some(span) = problem.span() {
            if let Some(text) = pointer(sql, span) {
                let _ = writeln!(self.err);
                let _ = write!(self.err, "{text}");
            }
        }
        let _ = self.err.flush();
    }

    /// Runs one dot command.
    fn run_dot(&mut self, line: &str) -> Stop {
        let mut words = split(line);
        if words.is_empty() {
            return Stop::Done;
        }
        let name = words.remove(0);
        let argument = |at: usize| words.get(at).cloned().unwrap_or_default();
        match name.as_str() {
            ".quit" | ".exit" => return Stop::Failed,
            ".help" => {
                let _ = write!(self.out, "{}", crate::help::DOT_COMMANDS);
            }
            ".mode" => {
                if words.is_empty() {
                    let _ =
                        writeln!(self.out, "current output mode: {}", self.settings.format.name());
                } else if let Some(format) = Format::from_name(&argument(0)) {
                    self.settings.set_format(format);
                    if let Some(table) = words.get(1) {
                        self.settings.table = table.clone();
                    }
                } else {
                    return self.complain(&format!(
                        "Error: mode should be one of: {}",
                        crate::help::MODES
                    ));
                }
            }
            ".headers" | ".header" => self.settings.header = on(&argument(0)),
            ".separator" => {
                self.settings.separator = argument(0);
                if let Some(newline) = words.get(1) {
                    self.settings.newline = newline.clone();
                }
            }
            ".nullvalue" | ".nullValue" => self.settings.nullvalue = argument(0),
            ".timer" => self.timer = on(&argument(0)),
            ".echo" => self.echo = on(&argument(0)),
            ".bail" => self.bail = on(&argument(0)),
            ".print" => {
                let _ = writeln!(self.out, "{}", words.join(" "));
            }
            ".read" => return self.run_file(Path::new(&argument(0))),
            ".output" => return self.redirect(words.first().map(String::as_str)),
            ".tables" => self.tables(words.first().map(String::as_str)),
            ".schema" => self.schema(words.first().map(String::as_str)),
            ".databases" => {
                let _ = writeln!(self.out, "memory:");
            }
            ".show" => self.show(),
            ".open" => {
                // The library decides what a name means, so `.open :memory:` is a new empty
                // database here the same way it is for a program, and a file is the library's
                // sentence about the format that is missing rather than a second one written here.
                match Database::open(&argument(0)) {
                    Ok(database) => {
                        self.connection = database.connect();
                        self.database = database;
                    }
                    Err(problem) => return self.complain(&format!("Error: {}", problem.message())),
                }
            }
            other => {
                return self
                    .complain(&format!("Error: unknown command or invalid arguments:  \"{}\". Enter \".help\" for help", other.trim_start_matches('.')));
            }
        }
        let _ = self.out.flush();
        Stop::Done
    }

    /// Prints a complaint about a dot command, which is an error like any other.
    fn complain(&mut self, message: &str) -> Stop {
        let _ = writeln!(self.err, "{message}");
        let _ = self.err.flush();
        self.after_error()
    }

    /// `.output`, both directions.
    fn redirect(&mut self, path: Option<&str>) -> Stop {
        let _ = self.out.flush();
        match path {
            None | Some("stdout") => {
                if let Some(given) = self.given.take() {
                    self.out = Sink::Given(given);
                }
            }
            Some(path) => {
                let path = PathBuf::from(path);
                match File::create(&path) {
                    Ok(file) => {
                        let opened = Sink::File(BufWriter::new(file), path);
                        if let Sink::Given(given) = std::mem::replace(&mut self.out, opened) {
                            self.given = Some(given);
                        }
                    }
                    Err(problem) => {
                        return self.complain(&format!(
                            "Error: cannot open \"{}\": {problem}",
                            path.display()
                        ));
                    }
                }
            }
        }
        Stop::Done
    }

    /// `.tables`, one name per line.
    fn tables(&mut self, pattern: Option<&str>) {
        let mut names = self.database.table_names();
        names.sort();
        for name in names {
            if pattern.is_none_or(|pattern| matches(&name, pattern)) {
                let _ = writeln!(self.out, "{name}");
            }
        }
    }

    /// `.schema`, the `CREATE TABLE` for every table or for one of them.
    fn schema(&mut self, wanted: Option<&str>) {
        let mut names = self.database.table_names();
        names.sort();
        for name in names {
            if wanted.is_some_and(|wanted| !matches(&name, wanted)) {
                continue;
            }
            if let Ok(sql) = self.database.table_sql(&name) {
                let _ = writeln!(self.out, "{sql}");
            }
        }
    }

    /// `.show`, in the order and the spacing DuckDB prints it.
    ///
    /// `width` is blank because there is no column width setting yet, and it is listed anyway so
    /// that a script reading this output finds the line where it expects it.
    fn show(&mut self) {
        let mut out = String::new();
        let _ = writeln!(out, "        echo: {}", off_on(self.echo));
        let _ = writeln!(out, "     headers: {}", off_on(self.settings.header));
        let _ = writeln!(out, "        mode: {}", self.settings.format.name());
        let _ = writeln!(out, "   nullvalue: \"{}\"", self.settings.nullvalue);
        let _ = writeln!(out, "      output: {}", self.output_name());
        let _ = writeln!(out, "colseparator: \"{}\"", escaped(&self.settings.separator));
        let _ = writeln!(out, "rowseparator: \"{}\"", escaped(&self.settings.newline));
        let _ = writeln!(out, "       width: ");
        let _ = writeln!(out, "    filename: {}", self.filename);
        let _ = write!(self.out, "{out}");
    }

    /// What `.show` calls the place output is going.
    fn output_name(&self) -> String {
        match &self.out {
            Sink::Given(_) => "stdout".to_string(),
            Sink::File(_, path) => path.display().to_string(),
        }
    }
}

/// Whether a name matches a `.tables` or `.schema` pattern, where `%` stands for any run.
fn matches(name: &str, pattern: &str) -> bool {
    let pattern = pattern.trim_matches('\'');
    if let Some(prefix) = pattern.strip_suffix('%') {
        name.starts_with(prefix)
    } else {
        name.eq_ignore_ascii_case(pattern)
    }
}

/// How a dot command's arguments are split: on whitespace, with quoted runs kept together.
fn split(line: &str) -> Vec<String> {
    let mut words = Vec::new();
    let mut current = String::new();
    let mut quote = None;
    let mut started = false;
    for character in line.chars() {
        match quote {
            Some(open) if character == open => quote = None,
            Some(_) => current.push(character),
            None if character == '\'' || character == '"' => {
                quote = Some(character);
                started = true;
            }
            None if character.is_whitespace() => {
                if started || !current.is_empty() {
                    words.push(std::mem::take(&mut current));
                    started = false;
                }
            }
            None => current.push(character),
        }
    }
    if started || !current.is_empty() {
        words.push(current);
    }
    words
}

/// How a dot command spells a boolean, where anything that is not a recognized "off" is "on".
fn on(word: &str) -> bool {
    !matches!(word, "off" | "0" | "false" | "no")
}

/// How `.show` spells one back.
fn off_on(flag: bool) -> &'static str {
    if flag { "on" } else { "off" }
}

/// The `LINE n:` and caret that go under an error message.
///
/// `None` when the span does not point into the text, which happens for an error raised about a
/// statement the caller did not hand us, and printing a caret under the wrong thing is worse than
/// printing none.
fn pointer(sql: &str, span: Span) -> Option<String> {
    let start = span.start as usize;
    if start > sql.len() || !sql.is_char_boundary(start) {
        return None;
    }
    let before = &sql[..start];
    let number = before.matches('\n').count() + 1;
    let line_start = before.rfind('\n').map_or(0, |at| at + 1);
    let line_end = sql[line_start..].find('\n').map_or(sql.len(), |at| line_start + at);
    let line = &sql[line_start..line_end];
    let prefix = format!("LINE {number}: ");
    let column = sql[line_start..start].chars().count();
    Some(format!("{prefix}{line}\n{}^\n", " ".repeat(prefix.chars().count() + column)))
}

#[cfg(test)]
mod tests {
    use super::{matches, on, pointer, split};
    use rudb::Span;

    #[test]
    fn a_dot_command_splits_on_whitespace() {
        assert_eq!(split(".mode csv"), vec![".mode", "csv"]);
        assert_eq!(split("  .timer   on  "), vec![".timer", "on"]);
    }

    #[test]
    fn a_quoted_argument_keeps_its_spaces() {
        assert_eq!(split(".separator ' | '"), vec![".separator", " | "]);
        assert_eq!(split(".nullvalue \"\""), vec![".nullvalue", ""]);
    }

    #[test]
    fn off_is_the_only_way_to_turn_something_off() {
        assert!(on("on"));
        assert!(on(""));
        assert!(!on("off"));
        assert!(!on("0"));
    }

    #[test]
    fn a_pattern_ending_in_a_percent_is_a_prefix() {
        assert!(matches("orders", "orders"));
        assert!(matches("orders", "ORDERS"));
        assert!(matches("orders", "ord%"));
        assert!(!matches("orders", "lineitem"));
    }

    #[test]
    fn the_caret_lands_under_the_span() {
        let sql = "SELECT nosuch";
        let text = pointer(sql, Span::new(7, 13)).expect("a pointer");
        assert_eq!(text, "LINE 1: SELECT nosuch\n               ^\n");
    }

    #[test]
    fn the_caret_counts_lines() {
        let sql = "SELECT\n  nosuch";
        let text = pointer(sql, Span::new(9, 15)).expect("a pointer");
        assert_eq!(text, "LINE 2:   nosuch\n          ^\n");
    }

    #[test]
    fn a_span_past_the_end_gets_no_pointer() {
        assert!(pointer("SELECT 1", Span::new(100, 101)).is_none());
    }
}
