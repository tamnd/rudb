//! The command line, in DuckDB's spelling.
//!
//! DuckDB's shell descends from SQLite's, which means single dash long options, a positional
//! argument that is the database rather than a script, and a second positional argument that is
//! SQL. None of that is what a Rust program would choose and all of it is what a script written
//! against `duckdb` expects, so it is what this parses.

use std::path::PathBuf;

use crate::format::Format;

/// One thing to run before the shell reads its input.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Command {
    /// SQL, or a dot command, given on the command line.
    Sql(String),
    /// A file of them.
    File(PathBuf),
}

/// What the shell was asked to do.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Action {
    /// Open a database and run.
    Run(Box<Options>),
    /// Print the version and stop.
    Version,
    /// Print the usage and stop.
    Help,
    /// Print the build configuration and stop.
    Config,
    /// The command line does not make sense, and this says why.
    Wrong(String),
}

/// Everything the command line can set.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Options {
    /// The database to open. `:memory:` until there is a storage format to open a file with.
    pub database: String,
    /// What to run before reading input, in the order it was given.
    pub commands: Vec<Command>,
    /// Whether to stop after the commands rather than reading input.
    pub stop_after_commands: bool,
    /// Set by `-interactive` and `-batch`, which override the guess made from whether input is a
    /// terminal.
    pub interactive: Option<bool>,
    /// Print each statement before running it.
    pub echo: bool,
    /// Stop at the first error even when reading a script.
    pub bail: bool,
    /// Open without allowing writes.
    pub readonly: bool,
    /// How results are printed, and everything that goes with it.
    pub settings: crate::format::Settings,
}

impl Default for Options {
    fn default() -> Self {
        Self {
            database: ":memory:".to_string(),
            commands: Vec::new(),
            stop_after_commands: false,
            interactive: None,
            echo: false,
            bail: false,
            readonly: false,
            settings: crate::format::Settings::default(),
        }
    }
}

/// Reads the command line.
///
/// Unknown options are an error rather than a positional argument. SQLite treats an unrecognized
/// dash argument as a filename and DuckDB inherits that, which turns a typo into a database called
/// `-csvv`, so this is one of the few places the shell deliberately does not copy the behaviour.
pub fn parse(arguments: &[String]) -> Action {
    let mut options = Options::default();
    let mut positional = 0;
    let mut at = 0;
    while at < arguments.len() {
        let argument = arguments[at].as_str();
        at += 1;
        let mut next = |name: &str| -> Result<String, String> {
            if at < arguments.len() {
                let value = arguments[at].clone();
                at += 1;
                Ok(value)
            } else {
                Err(format!("{name} wants a value"))
            }
        };
        match argument {
            "-version" | "--version" | "-V" => return Action::Version,
            "-h" | "-help" | "--help" => return Action::Help,
            "--print-config" => return Action::Config,
            "-c" | "-s" | "--command" => match next(argument) {
                Ok(sql) => {
                    options.commands.push(Command::Sql(sql));
                    options.stop_after_commands = true;
                }
                Err(why) => return Action::Wrong(why),
            },
            "-cmd" => match next(argument) {
                Ok(sql) => options.commands.push(Command::Sql(sql)),
                Err(why) => return Action::Wrong(why),
            },
            "-f" | "-file" => match next(argument) {
                Ok(path) => {
                    options.commands.push(Command::File(PathBuf::from(path)));
                    options.stop_after_commands = true;
                }
                Err(why) => return Action::Wrong(why),
            },
            "-init" => match next(argument) {
                Ok(path) => options.commands.push(Command::File(PathBuf::from(path))),
                Err(why) => return Action::Wrong(why),
            },
            "-separator" => match next(argument) {
                Ok(value) => options.settings.separator = value,
                Err(why) => return Action::Wrong(why),
            },
            "-newline" => match next(argument) {
                Ok(value) => options.settings.newline = value,
                Err(why) => return Action::Wrong(why),
            },
            "-nullvalue" => match next(argument) {
                Ok(value) => options.settings.nullvalue = value,
                Err(why) => return Action::Wrong(why),
            },
            "-header" => options.settings.header = true,
            "-noheader" => options.settings.header = false,
            "-echo" => options.echo = true,
            "-bail" => options.bail = true,
            "-readonly" => options.readonly = true,
            "-interactive" => options.interactive = Some(true),
            "-batch" => options.interactive = Some(false),
            "-no-stdin" => options.stop_after_commands = true,
            "-no-init" | "-unsigned" | "-unredacted" | "-safe" => {}
            other if other.starts_with('-') => {
                match Format::from_name(other.trim_start_matches('-')) {
                    Some(format) => options.settings.set_format_flag(format),
                    None => return Action::Wrong(format!("unknown option {other}")),
                }
            }
            other => {
                positional += 1;
                match positional {
                    1 => options.database = other.to_string(),
                    2 => {
                        options.commands.push(Command::Sql(other.to_string()));
                        options.stop_after_commands = true;
                    }
                    _ => return Action::Wrong(format!("too many arguments, starting at {other}")),
                }
            }
        }
    }
    Action::Run(Box::new(options))
}

#[cfg(test)]
mod tests {
    use super::{Action, Command, parse};
    use crate::format::Format;

    fn options(arguments: &[&str]) -> super::Options {
        let owned: Vec<String> = arguments.iter().map(|text| (*text).to_string()).collect();
        match parse(&owned) {
            Action::Run(options) => *options,
            other => panic!("expected a run, got {other:?}"),
        }
    }

    #[test]
    fn nothing_means_an_interactive_memory_database() {
        let parsed = options(&[]);
        assert_eq!(parsed.database, ":memory:");
        assert!(parsed.commands.is_empty());
        assert!(!parsed.stop_after_commands);
    }

    #[test]
    fn a_command_runs_and_stops() {
        let parsed = options(&["-c", "SELECT 1"]);
        assert_eq!(parsed.commands, vec![Command::Sql("SELECT 1".to_string())]);
        assert!(parsed.stop_after_commands);
    }

    #[test]
    fn commands_keep_their_order() {
        let parsed = options(&["-c", "one", "-c", "two"]);
        assert_eq!(
            parsed.commands,
            vec![Command::Sql("one".to_string()), Command::Sql("two".to_string())]
        );
    }

    #[test]
    fn cmd_runs_first_and_does_not_stop() {
        let parsed = options(&["-cmd", ".mode csv"]);
        assert!(!parsed.stop_after_commands);
    }

    #[test]
    fn the_first_positional_is_the_database_and_the_second_is_sql() {
        let parsed = options(&["shop.db", "SELECT 1"]);
        assert_eq!(parsed.database, "shop.db");
        assert_eq!(parsed.commands, vec![Command::Sql("SELECT 1".to_string())]);
        assert!(parsed.stop_after_commands);
    }

    #[test]
    fn a_mode_flag_sets_the_mode_and_its_separator() {
        let parsed = options(&["-csv"]);
        assert_eq!(parsed.settings.format, Format::Csv);
        assert_eq!(parsed.settings.separator, ",");
    }

    #[test]
    fn a_mode_flag_leaves_the_row_separator_where_it_was_and_the_dot_command_does_not() {
        // `duckdb -csv` ends a row with a newline and `.mode csv` ends it with a carriage return
        // and a newline, on the same build. Both were read off `duckdb v2.0.0-dev84237`.
        let parsed = options(&["-csv"]);
        assert_eq!(parsed.settings.newline, "\n");
        let mut settings = parsed.settings;
        settings.set_format(Format::Csv);
        assert_eq!(settings.newline, "\r\n");
    }

    #[test]
    fn a_separator_given_after_the_mode_wins() {
        let parsed = options(&["-csv", "-separator", ";"]);
        assert_eq!(parsed.settings.separator, ";");
    }

    #[test]
    fn an_unknown_option_is_an_error_rather_than_a_filename() {
        assert!(matches!(parse(&["-csvv".to_string()]), Action::Wrong(_)));
    }

    #[test]
    fn an_option_missing_its_value_says_so() {
        assert!(matches!(parse(&["-c".to_string()]), Action::Wrong(_)));
    }

    #[test]
    fn version_and_help_win_wherever_they_appear() {
        assert!(matches!(parse(&["-csv".to_string(), "-version".to_string()]), Action::Version));
        assert!(matches!(parse(&["-help".to_string()]), Action::Help));
    }
}
