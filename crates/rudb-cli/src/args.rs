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
    /// What `--set name=value` asked for, in the order it was given.
    ///
    /// Kept apart from [`Options::commands`] rather than pushed in as SQL, because these run
    /// before everything else whatever position they were written in. A flag that configures the
    /// engine and a flag that runs a query are two different things, and a benchmark script that
    /// puts its `--set` at the end of the line means the same thing as one that puts it first.
    pub sets: Vec<String>,
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
            sets: Vec::new(),
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
            // Two dashes, like `--print-config`, because DuckDB has no flag of this name and the
            // single dash forms in this list are the ones a script written against `duckdb`
            // already uses. A name that is ours should look like it.
            "--set" => match next(argument) {
                Ok(pair) => match pair.split_once('=') {
                    Some(_) => options.sets.push(pair),
                    None => {
                        return Action::Wrong(format!("--set is written name=value, not {pair}"));
                    }
                },
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
                match Format::from_flag(other.trim_start_matches('-')) {
                    Some(format) => options.settings.set_format_flag(format),
                    None => return Action::Wrong(format!("unknown option {other}")),
                }
            }
            // The first one is the database and every one after it is SQL, however many there are.
            // There is no count to get wrong: `duckdb a.db "SELECT 1" extra` does not complain
            // about the third argument, it runs it, and says the table `extra` does not exist.
            // Per #246.
            other => {
                positional += 1;
                if positional == 1 {
                    options.database = other.to_string();
                } else {
                    options.commands.push(Command::Sql(other.to_string()));
                    options.stop_after_commands = true;
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

    /// Every positional after the first is another statement, in the order they were written.
    ///
    /// DuckDB has no limit here and no error for the count, so neither does this. Per #246.
    #[test]
    fn every_positional_after_the_database_is_another_statement() {
        let parsed = options(&["shop.db", "SELECT 1", "SELECT 2", "SELECT 3"]);
        assert_eq!(parsed.database, "shop.db");
        assert_eq!(
            parsed.commands,
            vec![
                Command::Sql("SELECT 1".to_string()),
                Command::Sql("SELECT 2".to_string()),
                Command::Sql("SELECT 3".to_string()),
            ]
        );
        assert!(parsed.stop_after_commands);
    }

    #[test]
    fn a_mode_flag_sets_the_mode_and_its_separator() {
        let parsed = options(&["-csv"]);
        assert_eq!(parsed.settings.format, Format::Csv);
        assert_eq!(parsed.settings.separator, ",");
    }

    /// The row separator is the one thing the csv flag does not set, which is DuckDB's behaviour.
    ///
    /// `duckdb -csv` writes `\n` at the end of a row and `duckdb -cmd ".mode csv"` writes `\r\n`,
    /// on the same build in the same run, and `tests/shell.rs` holds both captures. This is the
    /// parse side of it.
    #[test]
    fn a_mode_flag_leaves_the_row_separator_where_it_was_and_the_dot_command_does_not() {
        assert_eq!(options(&["-csv"]).settings.newline, "\n");
        assert_eq!(options(&["-csv", "-newline", ";"]).settings.newline, ";");
    }

    /// What each flag sets, against `duckdb v2.0.0-dev84237` read out of `.show`.
    ///
    /// The separators are given first so that a flag which leaves one alone can be told apart from
    /// one that sets it to the value it already had. Per #239.
    #[test]
    fn each_mode_flag_sets_the_separators_that_flag_sets_and_no_others() {
        let given = |flag: &str| {
            let parsed = options(&["-separator", ";", "-newline", "@", flag]);
            (parsed.settings.separator, parsed.settings.newline)
        };
        assert_eq!(given("-ascii"), ("\u{1f}".to_string(), "\u{1e}".to_string()));
        assert_eq!(given("-csv"), (",".to_string(), "@".to_string()));
        let neither = [
            "-box",
            "-column",
            "-html",
            "-json",
            "-jsonlines",
            "-line",
            "-list",
            "-markdown",
            "-quote",
            "-table",
        ];
        for flag in neither {
            assert_eq!(given(flag), (";".to_string(), "@".to_string()), "{flag}");
        }
    }

    /// The four modes that are not flags, per #238.
    ///
    /// Each of them is still a mode, so `.mode tabs` works and `-tabs` does not, which is what the
    /// binary does. The aliases are not flags either.
    #[test]
    fn a_mode_that_duckdb_has_no_flag_for_is_an_error_here_too() {
        for flag in ["-duckbox", "-insert", "-tabs", "-trash", "-lines", "-tsv", "-ndjson"] {
            assert!(matches!(parse(&[flag.to_string()]), Action::Wrong(_)), "{flag}");
        }
    }

    #[test]
    fn a_separator_given_after_the_mode_wins() {
        let parsed = options(&["-csv", "-separator", ";"]);
        assert_eq!(parsed.settings.separator, ";");
    }

    #[test]
    fn every_set_flag_is_kept_in_order_and_apart_from_the_sql() {
        let parsed = options(&["--set", "hash.table=unchained", "-c", "SELECT 1", "--set", "x=y"]);
        assert_eq!(parsed.sets, ["hash.table=unchained", "x=y"]);
        assert_eq!(parsed.commands, [Command::Sql("SELECT 1".to_string())]);
    }

    #[test]
    fn a_set_flag_without_a_value_says_how_it_is_written() {
        assert!(matches!(
            parse(&["--set".to_string(), "hash.table".to_string()]),
            Action::Wrong(why) if why.contains("name=value")
        ));
        assert!(matches!(parse(&["--set".to_string()]), Action::Wrong(_)));
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
