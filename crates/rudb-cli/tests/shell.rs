//! The shell against a real DuckDB.
//!
//! Everything in `testdata/` was produced by running the `duckdb` binary, by `testdata/capture.sh`,
//! and `testdata/duckdb-version.txt` records which binary it was. Nothing here compares rudb
//! against a description of DuckDB's behaviour written from memory, because that is how a
//! compatibility claim quietly stops being true.
//!
//! The version on record is the v2.0 alpha at the commit the grammar is vendored from, which is the
//! binary `scripts/oracle` installs. It is not a build anybody can get with a package manager, so
//! the capture has to happen on a machine that has one, and the files here came off server2 rather
//! than off a laptop. That is also why one of them changed shape when it moved: `.mode csv` ends its
//! lines with a carriage return and a newline on Linux and the file captured on macOS had neither
//! the carriage returns nor a reason for not having them. rudb writes the carriage return, so the
//! goldens now say what rudb says and what the binary this project pins says.

use std::io::Write;
use std::process::ExitCode;
use std::sync::{Arc, Mutex};

/// A `Write` that keeps what was written, so a test can read both streams without a pipe.
#[derive(Clone, Default)]
struct Captured(Arc<Mutex<Vec<u8>>>);

impl Captured {
    fn text(&self) -> String {
        String::from_utf8(self.0.lock().expect("not poisoned").clone()).expect("valid utf8")
    }
}

impl Write for Captured {
    fn write(&mut self, buffer: &[u8]) -> std::io::Result<usize> {
        self.0.lock().expect("not poisoned").extend_from_slice(buffer);
        Ok(buffer.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

/// Runs the shell and gives back what it printed, what it complained about, and whether it failed.
fn run(arguments: &[&str]) -> (String, String, bool) {
    let owned: Vec<String> = arguments.iter().map(|text| (*text).to_string()).collect();
    let out = Captured::default();
    let err = Captured::default();
    let code = rudb_cli::run(&owned, Box::new(out.clone()), Box::new(err.clone()));
    let failed = format!("{code:?}") != format!("{:?}", ExitCode::SUCCESS);
    (out.text(), err.text(), failed)
}

/// The path to a file in `testdata/`.
fn testdata(name: &str) -> String {
    format!("{}/testdata/{name}", env!("CARGO_MANIFEST_DIR"))
}

/// What is in one.
fn golden(name: &str) -> String {
    std::fs::read_to_string(testdata(name)).unwrap_or_else(|why| panic!("read {name}: {why}"))
}

/// Compares two blocks of output, ignoring trailing spaces on each line.
///
/// DuckDB pads the `duckbox` footer out to the width of the table, so the line ends in spaces that
/// no terminal and no diff tool shows. Reproducing them exactly is possible and pointless, and a
/// test that fails on invisible bytes teaches people to ignore it, so the comparison is on the part
/// anybody can see. Everything else in this file is compared byte for byte.
fn same_ignoring_trailing_space(left: &str, right: &str) -> bool {
    let trimmed = |text: &str| {
        text.lines().map(|line| line.trim_end_matches(' ').to_string()).collect::<Vec<_>>()
    };
    trimmed(left) == trimmed(right)
}

/// The modes DuckDB was captured in, which is all of them except `trash`.
const MODES: &[&str] = &[
    "duckbox",
    "box",
    "table",
    "markdown",
    "line",
    "list",
    "csv",
    "tabs",
    "json",
    "jsonlines",
    "quote",
    "insert",
    "html",
    "ascii",
    "column",
];

#[test]
fn every_mode_prints_what_duckdb_prints() {
    let mut wrong = Vec::new();
    for mode in MODES {
        let (out, err, failed) = run(&[
            "-f",
            &testdata("setup.sql"),
            "-cmd",
            &format!(".mode {mode}"),
            "-c",
            "SELECT * FROM t ORDER BY a",
        ]);
        assert!(!failed, "mode {mode} failed: {err}");
        let want = golden(&format!("mode-{mode}.txt"));
        if !same_ignoring_trailing_space(&out, &want) {
            wrong.push(format!("--- {mode}\nwant:\n{want}\ngot:\n{out}"));
        }
    }
    assert!(wrong.is_empty(), "{}", wrong.join("\n"));
}

/// A database and two statements on the command line, which is three positional arguments.
///
/// DuckDB has no limit on how many there are and no error for the count. rudb refused the third one
/// until #246, which is the kind of thing nobody finds out about until a script that has worked for
/// years is handed to a different binary.
#[test]
fn every_positional_after_the_database_is_another_statement() {
    let (out, err, failed) = run(&[":memory:", "SELECT 1 AS a", "SELECT 2 AS b"]);
    assert!(!failed, "three positionals failed: {err}");
    let want = golden("positionals.txt");
    assert!(same_ignoring_trailing_space(&out, &want), "want:\n{want}\ngot:\n{out}");
}

/// The counts under a `duckbox` table, and the row of dots that stands in for what it left out.
///
/// One query per line of `counts.sql` against the file of the same number, and between them they
/// cover every shape the footer has: no footer at all under nine rows, a row count on its own, a row
/// count a box is widened to fit, a count of what was shown on its own line and merged onto the one
/// above it, a column count beside it, and the hint in the gap between the two. The dots are in
/// there too, both the alignment of them and the fact that the value they line up under is the
/// shorter of the two either side of the gap rather than the shortest on show.
#[test]
fn the_counts_under_a_table_are_the_ones_duckdb_prints() {
    let queries = golden("counts.sql");
    let mut wrong = Vec::new();
    for (at, query) in queries.lines().enumerate() {
        let (out, err, failed) = run(&["-c", query]);
        assert!(!failed, "{query} failed: {err}");
        let want = golden(&format!("counts-{}.txt", at + 1));
        if !same_ignoring_trailing_space(&out, &want) {
            wrong.push(format!("--- {query}\nwant:\n{want}\ngot:\n{out}"));
        }
    }
    assert!(wrong.is_empty(), "{}", wrong.join("\n"));
}

#[test]
fn an_empty_result_prints_its_columns_and_a_row_count() {
    let (out, _, failed) =
        run(&["-f", &testdata("setup.sql"), "-c", "SELECT * FROM t WHERE a > 9000"]);
    assert!(!failed);
    assert!(
        same_ignoring_trailing_space(&out, &golden("empty.txt")),
        "want:\n{}\ngot:\n{out}",
        golden("empty.txt")
    );
}

#[test]
fn show_reports_the_settings_the_way_duckdb_does() {
    let (out, _, failed) = run(&["-c", ".show"]);
    assert!(!failed);
    assert_eq!(out, golden("show.txt"));
}

#[test]
fn a_query_on_the_command_line_prints_and_exits_zero() {
    let (out, err, failed) = run(&["-c", "SELECT 42 AS answer"]);
    assert!(!failed, "{err}");
    assert!(out.contains("42"), "{out}");
}

#[test]
fn a_syntax_error_goes_to_stderr_with_a_caret_and_exits_one() {
    let (out, err, failed) = run(&["-c", "SELECT FROM"]);
    assert!(failed);
    assert!(out.is_empty(), "nothing should have been printed, got {out}");
    assert!(err.contains("Parser Error"), "{err}");
    assert!(err.contains("LINE 1: SELECT FROM"), "{err}");
    assert!(err.contains('^'), "{err}");
}

/// A binder error prints its message and sets the exit code, and it prints no caret.
///
/// DuckDB points at the column it could not find. rudb cannot yet, because the binder raises its
/// errors without a span, and a caret under the wrong token would be worse than none. The shell
/// side of it is done: as soon as the binder attaches spans this prints the same three lines a
/// parser error does, which is what the test above already checks.
#[test]
fn a_binder_error_says_what_is_wrong_and_exits_one() {
    let (out, err, failed) = run(&["-c", "SELECT nosuch"]);
    assert!(failed);
    assert!(out.is_empty(), "nothing should have been printed, got {out}");
    assert!(err.contains("Binder Error"), "{err}");
    assert!(err.contains("nosuch"), "{err}");
}

#[test]
fn a_second_statement_does_not_run_after_an_error() {
    let (out, _, failed) = run(&["-c", "SELECT nosuch; SELECT 42 AS answer"]);
    assert!(failed);
    assert!(!out.contains("42"), "the second statement ran: {out}");
}

#[test]
fn a_file_that_is_not_there_is_an_error() {
    let (_, err, failed) = run(&["-f", "/no/such/script.sql"]);
    assert!(failed);
    assert!(err.contains("no/such/script.sql"), "{err}");
}

#[test]
fn a_database_file_says_there_is_no_storage_format_yet() {
    let (_, err, failed) = run(&["shop.db", "SELECT 1"]);
    assert!(failed);
    assert!(err.contains("no storage format"), "{err}");
}

#[test]
fn a_script_runs_its_dot_commands_and_its_sql_in_order() {
    let (out, err, failed) = run(&["-f", &testdata("script.sql")]);
    assert!(!failed, "{err}");
    assert_eq!(out, "a,b,c\r\n1,one,1.5\r\n2,two,2.25\r\n30,a longer one,-3.0\r\n3\n");
}

#[test]
fn read_runs_another_script() {
    let (out, err, failed) = run(&[
        "-cmd",
        ".mode list",
        "-c",
        &format!(".read {}\nSELECT count(*) FROM t;", testdata("setup.sql")),
    ]);
    assert!(!failed, "{err}");
    assert_eq!(out, "count_star()\n3\n");
}

#[test]
fn a_statement_can_be_spread_over_several_lines() {
    let (out, err, failed) = run(&["-cmd", ".mode list", "-c", "SELECT\n  1 AS a,\n  2 AS b;"]);
    assert!(!failed, "{err}");
    assert_eq!(out, "a|b\n1|2\n");
}

#[test]
fn echo_prints_each_statement_before_running_it() {
    let (out, _, failed) = run(&["-echo", "-cmd", ".mode list", "-c", "SELECT 1 AS a"]);
    assert!(!failed);
    assert_eq!(out, "SELECT 1 AS a\na\n1\n");
}

#[test]
fn tables_and_schema_read_the_catalog() {
    let (out, err, failed) =
        run(&["-cmd", "CREATE TABLE q(x INTEGER, y VARCHAR)", "-c", ".tables\n.schema q"]);
    assert!(!failed, "{err}");
    assert_eq!(out, "q\nCREATE TABLE q(x INTEGER, y VARCHAR);\n");
}

/// Which bytes put quotes around a CSV field.
///
/// A separate golden from the mode capture because the mode capture is three tidy rows and none of
/// the bytes that decide this appear in it. The rule is not RFC 4180 and it is wider than anybody
/// guesses: an apostrophe, a delete, a tab and every byte in the top half all quote on their own.
/// That last one is the one that matters, because thirty two of the forty three ClickBench queries
/// return Russian text and a differential run against DuckDB reports every one of them as different
/// while the numbers underneath are right.
#[test]
fn a_csv_field_is_quoted_on_the_bytes_duckdb_quotes_it_on() {
    let query = golden("quoting.sql");
    let (out, err, failed) = run(&["-cmd", ".mode csv", "-c", query.trim_end()]);
    assert!(!failed, "{err}");
    assert_eq!(out, golden("quoting.txt"));
}

/// Every shape of `DESCRIBE`, byte for byte against the pinned binary.
///
/// The six columns are an interface rather than a print. `rudb-compat` asks DuckDB what types a
/// result has by running `SELECT column_name, column_type FROM (DESCRIBE <statement>)`, so the names
/// and the order of those columns are what a second engine reads, and the ninth line of
/// describe.sql is that exact query. The rest of the file pins the parts that are easy to get
/// almost right: `NO` survives a `SELECT *` off a `NOT NULL` column and does not survive arithmetic
/// on it, a decimal literal is `DECIMAL(2,1)` rather than `DOUBLE`, and a bare `NULL` has a type
/// whose name is spelled with the quotes in it.
#[test]
fn describe_prints_the_six_columns_duckdb_prints() {
    let (out, err, failed) = run(&[
        "-cmd",
        &format!(".read {}", testdata("setup.sql")),
        "-csv",
        "-c",
        &format!(".read {}", testdata("describe.sql")),
    ]);
    assert!(!failed, "{err}");
    let want = golden("describe.txt");
    assert!(same_ignoring_trailing_space(&out, &want), "want:\n{want}\ngot:\n{out}");
}

/// `-csv` ends a row with a newline and `.mode csv` ends it with a carriage return and a newline.
///
/// Both goldens come out of the same binary in the same run of `testdata/capture.sh`, so the pair
/// of them is the whole argument that this is DuckDB's behaviour rather than a capture accident. It
/// reads like an oversight upstream and copying it is still right, because the people who reach for
/// the flag are the people piping the output into something that counts bytes.
#[test]
fn a_mode_flag_and_the_dot_command_end_a_row_differently() {
    let query = golden("quoting.sql");
    let (out, err, failed) = run(&["-csv", "-c", query.trim_end()]);
    assert!(!failed, "{err}");
    assert_eq!(out, golden("quoting-flag.txt"));
    assert!(!out.contains('\r'), "the flag does not set the row separator");
}

/// The twelve modes DuckDB also has a command line flag for, in the order `duckdb -help` lists them.
const FLAGS: &[&str] = &[
    "ascii",
    "box",
    "column",
    "csv",
    "html",
    "json",
    "jsonlines",
    "line",
    "list",
    "markdown",
    "quote",
    "table",
];

/// Every mode name and every alias for one, which is what `capture.sh` probes the binary with.
///
/// The list is in both places because the capture runs on a machine with the pinned DuckDB on it
/// and this runs everywhere, and a name that falls out of one of the two shows up as a name in
/// `flags-refused.txt` that this does not know about.
const NAMES: &[&str] = &[
    "ascii",
    "box",
    "column",
    "csv",
    "duckbox",
    "html",
    "insert",
    "json",
    "jsonlines",
    "line",
    "lines",
    "list",
    "markdown",
    "ndjson",
    "quote",
    "table",
    "tabs",
    "trash",
    "tsv",
];

/// What each flag sets, against a capture of `.show` from the binary.
///
/// `duckdb -quote` writes `'a'|'b'` and `duckdb -cmd ".mode quote"` writes `'a','b'`, and `-ascii`
/// sets the row separator where `-csv` does not. Reading it out of `.show` rather than out of a
/// query is what makes the difference between leaving a separator alone and setting it to what it
/// already was visible. The separators are given first for the same reason. Per #239.
#[test]
fn every_mode_flag_leaves_the_settings_where_duckdb_leaves_them() {
    let mut wrong = Vec::new();
    for mode in FLAGS {
        let flag = format!("-{mode}");
        let (out, err, failed) = run(&["-separator", ";", "-newline", "@", &flag, "-c", ".show"]);
        assert!(!failed, "flag {mode} failed: {err}");
        let want = golden(&format!("flag-{mode}.txt"));
        if out != want {
            wrong.push(format!("--- {mode}\nwant:\n{want}\ngot:\n{out}"));
        }
    }
    assert!(wrong.is_empty(), "{}", wrong.join("\n"));
}

/// The names the binary has no flag for are the names this has no flag for.
///
/// Four of the sixteen modes are not flags upstream and neither are the three aliases, and this
/// shell used to accept all nineteen because it derived the flags from the mode table instead of
/// copying the list. A script written against `rudb -tabs` is a script that fails against the
/// binary with a suggestion list and a non zero exit. Per #238.
#[test]
fn a_mode_name_duckdb_has_no_flag_for_is_refused_here_too() {
    let captured = golden("flags-refused.txt");
    let refused: Vec<&str> =
        captured.lines().map(str::trim).filter(|line| !line.is_empty()).collect();
    for name in &refused {
        assert!(NAMES.contains(name), "{name} is refused by the binary and is not in NAMES");
    }
    for name in NAMES {
        let (_, err, failed) = run(&[&format!("-{name}"), "-c", "SELECT 1"]);
        assert_eq!(failed, refused.contains(name), "-{name}: {err}");
    }
}

#[test]
fn an_unknown_dot_command_is_an_error_and_the_run_fails() {
    let (_, err, failed) = run(&["-c", ".nonsense"]);
    assert!(failed);
    assert!(err.contains("unknown command"), "{err}");
}

#[test]
fn output_sends_results_to_a_file_and_back_again() {
    let path = std::env::temp_dir().join("rudb-shell-output-test.csv");
    let _ = std::fs::remove_file(&path);
    let script =
        format!(".mode csv\n.output {}\nSELECT 1 AS a;\n.output\nSELECT 2 AS a;", path.display());
    let (out, err, failed) = run(&["-c", &script]);
    assert!(!failed, "{err}");
    assert_eq!(std::fs::read_to_string(&path).expect("the file was written"), "a\r\n1\r\n");
    assert_eq!(out, "a\r\n2\r\n");
    let _ = std::fs::remove_file(&path);
}

#[test]
fn version_and_help_and_config_print_and_stop() {
    let (out, _, failed) = run(&["-version"]);
    assert!(!failed);
    assert_eq!(out, format!("rudb {}\n", env!("CARGO_PKG_VERSION")));

    let (out, _, failed) = run(&["-help"]);
    assert!(!failed);
    assert!(out.starts_with("Usage: rudb"), "{out}");

    let (out, _, failed) = run(&["--print-config"]);
    assert!(!failed);
    assert!(out.contains("vector-size: 1024"), "{out}");
    // The settings a run can change come from the config object rather than from a literal in the
    // shell, so that what this prints is what the engine was opened with.
    // Eighty percent of what the machine has, so the number is the machine's and not a literal,
    // and what is asserted is that a budget was read and printed rather than which one.
    assert!(out.contains("memory-limit: "), "{out}");
    assert!(!out.contains("memory-limit: unlimited"), "{out}");
    assert!(out.contains("query-timeout: none"), "{out}");
    assert!(out.contains("threads: "), "{out}");
}

#[test]
fn an_unknown_option_says_so_rather_than_opening_a_file_of_that_name() {
    let (_, err, failed) = run(&["-csvv"]);
    assert!(failed);
    assert!(err.contains("unknown option -csvv"), "{err}");
}
