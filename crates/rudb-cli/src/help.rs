//! The text the shell prints about itself.
//!
//! Kept together so that adding an option and forgetting to document it is one file to notice
//! rather than two.

/// The list of modes, which both `.help` and the error from a bad `.mode` want.
pub const MODES: &str = "ascii, box, column, csv, duckbox, html, insert, json, jsonlines, line, list, markdown, quote, table, tabs, trash";

/// What `-help` prints.
pub const USAGE: &str = "\
Usage: rudb [OPTIONS] [FILENAME [SQL...]]

An embedded analytical database, compatible with DuckDB.

FILENAME is the database to open. Only :memory: works today, because there is no storage format
yet. A second argument is SQL to run, after which the shell exits.

Options:
  -bail                  stop after the first error
  -batch                 read input as a script even when it is a terminal
  -c, -s SQL             run SQL and exit
  -cmd SQL               run SQL before reading input, and keep reading
  -echo                  print each statement before running it
  -f, -file FILENAME     run a file of SQL and exit
  -header, -noheader     turn column names on or off
  -init FILENAME         run a file of SQL before reading input
  -interactive           show a prompt even when input is not a terminal
  -newline SEP           what goes between rows in the separated modes
  -nullvalue TEXT        what a null prints as
  -readonly              open without allowing writes
  -separator SEP         what goes between values in the separated modes
  --set NAME=VALUE       run SET NAME = VALUE before anything else, repeatable
  -version               print the version and exit
  --print-config         print the build configuration and exit
  -h, -help              print this and exit

Most modes can also be given as an option: -ascii, -box, -column, -csv, -html, -json, -jsonlines,
-line, -list, -markdown, -quote and -table. They set the mode and they do not all set the
separators that .mode sets, which is what DuckDB does.
";

/// What `.help` prints.
pub const DOT_COMMANDS: &str = "\
.bail on|off             stop after the first error
.databases               list the attached databases
.echo on|off             print each statement before running it
.exit                    exit the shell
.headers on|off          turn column names on or off
.help                    print this
.mode MODE ?TABLE?       set the output mode
.nullvalue TEXT          set what a null prints as
.open FILENAME           close the current database and open another
.output ?FILENAME?       send output to a file, or back to stdout
.print TEXT...           print the text
.quit                    exit the shell
.read FILENAME           run a file of SQL
.schema ?PATTERN?        show the CREATE TABLE for the tables
.separator COL ?ROW?     set the separators used in the separated modes
.show                    show the current settings
.tables ?PATTERN?        list the tables
.timer on|off            print how long each statement took
";
