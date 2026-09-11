//! Every ClickBench query, unmodified, over a real Parquet file, against the answers duckdb gives.
//!
//! `crates/rudb/tests/clickbench.rs` asks whether the forty three queries plan. This asks the
//! question that matters, which is whether they come out with duckdb's numbers. The SQL is the
//! published file character for character, the data is a Parquet file read by the Parquet reader
//! rather than a table typed in by hand, and the expected answers were produced by the duckdb
//! binary over that same file. Nothing here is rudb checking its own homework.
//!
//! It lives in this crate because the comparison is over rendered output. The column names, the
//! column types as a shell spells them and the text of every value are all things a caller sees,
//! and all three have been wrong at some point in a way that a comparison of raw values would have
//! missed. The renderer that produces them is here, so the test is here, and it reaches across to
//! the fixture in `rudb` rather than keeping a second copy of a seven hundred kilobyte file.
//!
//! Three of the queries do not have one right answer and one does not have any. A LIMIT over groups
//! that tie leaves the engine free to return any of the tied rows, and q18 has a LIMIT with no
//! ORDER BY at all. `scripts/clickbench-answers` finds these by running duckdb three times over the
//! same rows in different orders and recording only what the three runs agreed on, so what is
//! asserted below is exactly what the query settles and nothing that it does not.

use rudb::Database;
use rudb_cli::Settings;
use rudb_cli::format::render;

/// The queries, as they come from the ClickBench repository by way of the suite in
/// `tamnd/rudb-bench`.
const SQL: &str = include_str!("../../rudb/testdata/clickbench.sql");

/// What duckdb answered for each of them, written by `scripts/clickbench-answers`.
const ANSWERS: &str = include_str!("../../rudb/testdata/clickbench-answers.txt");

/// The file the queries read, which the fixture generator in the same directory made.
const FIXTURE: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/../rudb/testdata/hits.parquet");

/// One query's expected answer, and how much of it duckdb settled.
struct Answer {
    name: String,
    /// `exact`, `last` or `count`, which the module documentation explains.
    mode: String,
    /// How many rows come back, which is settled for all three modes.
    rows: usize,
    names: Vec<String>,
    types: Vec<String>,
    /// The rows for `exact`, the last column of each row for `last`, and nothing for `count`.
    body: Vec<Vec<String>>,
}

/// The queries in the order the file writes them.
///
/// The DDL is skipped. This test loads the table from the Parquet file instead, which is what the
/// ClickBench loader for duckdb does and is the only version of the load that puts the reader under
/// the queries.
fn queries() -> Vec<(String, String)> {
    let mut found = Vec::new();
    let mut name = String::new();
    let mut sql = String::new();
    for line in SQL.lines() {
        let line = line.trim();
        if let Some(rest) = line.strip_prefix("--") {
            let rest = rest.trim();
            if rest.starts_with('q') && rest[1..].chars().all(|c| c.is_ascii_digit()) {
                name = rest.to_string();
            }
            continue;
        }
        if line.is_empty() {
            continue;
        }
        if !sql.is_empty() {
            sql.push(' ');
        }
        sql.push_str(line);
        if let Some(statement) = sql.strip_suffix(';') {
            if !name.is_empty() {
                found.push((name.clone(), statement.to_string()));
            }
            sql.clear();
        }
    }
    found
}

/// The answers file, parsed.
fn answers() -> Vec<Answer> {
    let mut found: Vec<Answer> = Vec::new();
    for line in ANSWERS.lines() {
        if let Some(rest) = line.strip_prefix("-- q") {
            let mut words = rest.split_whitespace();
            let number = words.next().expect("a query number");
            let mode = words.next().expect("a mode").to_string();
            let rows = words.next().expect("a row count").parse().expect("a number");
            found.push(Answer {
                name: format!("q{number}"),
                mode,
                rows,
                names: Vec::new(),
                types: Vec::new(),
                body: Vec::new(),
            });
            continue;
        }
        if line.starts_with("--") || line.is_empty() {
            continue;
        }
        let cells: Vec<String> = line.split('\t').map(str::to_string).collect();
        let answer = found.last_mut().expect("a row before any query was named");
        if answer.names.is_empty() {
            answer.names = cells;
        } else if answer.types.is_empty() {
            answer.types = cells;
        } else {
            answer.body.push(cells);
        }
    }
    found
}

/// The cells of a rendered `duckbox` table, names first, then types, then a row each.
///
/// The padding comes off, so this compares what the columns are called and what is in them rather
/// than how wide they were drawn. Width is worth testing and `rudb-compat` tests it, but a test
/// that fails on a column being one character narrower reports a spacing change as a wrong answer,
/// and this test is the one that has to be believed when it says the answer is wrong.
///
/// The counts under the table are outside it, so taking only the lines the box drew leaves them
/// behind on their own. That was worth saying while rudb did not write them and is worth saying now
/// that it does, because the reason this needs no filter is the shape of the output rather than luck.
fn cells(rendered: &str) -> Vec<Vec<String>> {
    let mut rows: Vec<Vec<String>> = Vec::new();
    for line in rendered.lines() {
        let Some(inner) = line.strip_prefix('│') else { continue };
        let inner = inner.strip_suffix('│').unwrap_or(inner);
        rows.push(inner.split('│').map(|cell| cell.trim().to_string()).collect());
    }
    rows
}

/// A database with the benchmark file loaded into `hits`.
///
/// The load is `CREATE TABLE AS SELECT`, which is how ClickBench loads duckdb, and it goes through
/// the replacement scan rather than through `read_parquet` because that is the spelling the
/// benchmark uses everywhere else.
fn loaded() -> Database {
    let database = Database::new();
    let sql = format!("CREATE TABLE hits AS SELECT * FROM '{FIXTURE}'");
    database.execute(&sql).expect("the benchmark fixture loads");
    assert_eq!(database.table_len("hits").expect("hits exists"), 10_000);
    database
}

#[test]
fn the_answers_line_up_with_the_queries() {
    let queries = queries();
    let answers = answers();
    assert_eq!(queries.len(), 43, "the query file does not hold forty three queries");
    let asked: Vec<&str> = queries.iter().map(|(name, _)| name.as_str()).collect();
    let answered: Vec<&str> = answers.iter().map(|answer| answer.name.as_str()).collect();
    assert_eq!(asked, answered, "the answers file is not the queries file, in order");
}

#[test]
fn every_query_answers_what_duckdb_answers() {
    let database = loaded();
    let answers = answers();
    let mut wrong = Vec::new();
    for ((name, sql), answer) in queries().into_iter().zip(&answers) {
        let result = match database.query(&sql) {
            Ok(result) => result,
            Err(error) => {
                wrong.push(format!("{name}: {error}"));
                continue;
            }
        };
        let got = cells(&render(&result, &Settings::default()));
        let (head, body) = got.split_at(2.min(got.len()));
        if head.len() != 2 {
            wrong.push(format!("{name}: no table came out at all"));
            continue;
        }
        if head[0] != answer.names {
            wrong.push(format!(
                "{name}: columns are {:?} and duckdb calls them {:?}",
                head[0], answer.names
            ));
            continue;
        }
        if head[1] != answer.types {
            wrong.push(format!(
                "{name}: types are {:?} and duckdb says {:?}",
                head[1], answer.types
            ));
            continue;
        }
        if body.len() != answer.rows {
            wrong.push(format!("{name}: {} rows where duckdb has {}", body.len(), answer.rows));
            continue;
        }
        match answer.mode.as_str() {
            "exact" => {
                if body != answer.body {
                    wrong.push(format!(
                        "{name}: rows are {:?} and duckdb has {:?}",
                        body, answer.body
                    ));
                }
            }
            "last" => {
                let last: Vec<Vec<String>> =
                    body.iter().map(|row| vec![row.last().cloned().unwrap_or_default()]).collect();
                if last != answer.body {
                    wrong.push(format!(
                        "{name}: the ordered column is {:?} and duckdb has {:?}",
                        last, answer.body
                    ));
                }
            }
            "count" => {}
            other => wrong.push(format!("{name}: the answers file says mode {other}")),
        }
    }
    assert!(
        wrong.is_empty(),
        "queries that do not answer what duckdb answers:\n{}",
        wrong.join("\n")
    );
}
