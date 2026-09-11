//! The shell against a real DuckDB.
//!
//! Everything in `testdata/` was produced by running the `duckdb` binary, by `testdata/capture.sh`,
//! and `testdata/duckdb-version.txt` records which binary it was. Nothing here compares rudb
//! against a description of DuckDB's behaviour written from memory, because that is how a
//! compatibility claim quietly stops being true.
//!
//! The version on record is a 1.x release rather than the v2.0 alpha the rest of the project
//! measures against. It is the right thing to diff output modes against anyway, since none of these
//! modes changed between them, and the capture moves to the pinned binary when
//! https://github.com/tamnd/rudb/issues/111 lands.

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
}

#[test]
fn an_unknown_option_says_so_rather_than_opening_a_file_of_that_name() {
    let (_, err, failed) = run(&["-csvv"]);
    assert!(failed);
    assert!(err.contains("unknown option -csvv"), "{err}");
}
