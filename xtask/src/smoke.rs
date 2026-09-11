//! The exit criterion of M0, as a command.
//!
//! `spec/17-milestones.md` ends M0 with a trivial query executing end to end on all three hosts, so
//! there has to be one command that runs that query and says whether it got the right answer. It is
//! here rather than in a test because the point is to run it on a machine that is not the one the
//! code was written on, and `cargo test` on a fresh checkout builds and runs the whole workspace,
//! which is a different and much longer question than does a query work here.
//!
//! What it checks is deliberately small and deliberately exact. Not that the engine is fast, not
//! that it is complete, but that on this host, with this toolchain, text turns into tokens, tokens
//! into a parse tree, the tree into a bound plan, the plan into operators and the operators into the
//! rows that are actually correct. A wrong answer here fails the command rather than printing a
//! table for somebody to read, because a check that needs a human to notice is not a check.

use std::fmt::Write as _;

use rudb::Database;
use rudb_common::{Field, LogicalType, Value};

/// Runs the milestone query and reports what happened.
pub(crate) fn run() -> Result<(), String> {
    println!("host: {} {}", std::env::consts::OS, std::env::consts::ARCH);
    println!("build: {}", if cfg!(debug_assertions) { "debug" } else { "release" });
    println!();

    let db = Database::new();
    db.create_table(
        "t",
        vec![Field::new("x", LogicalType::Integer), Field::new("label", LogicalType::Varchar)],
    )
    .map_err(|e| format!("could not create the table: {e}"))?;

    let rows: Vec<Vec<Value>> =
        (1..=10).map(|x| vec![Value::Integer(x), Value::Varchar(format!("row {x}"))]).collect();
    db.append("t", &rows).map_err(|e| format!("could not append rows: {e}"))?;

    // The query from the milestone, spelled exactly as the milestone spells it.
    check(
        &db,
        "SELECT * FROM t WHERE x > 5",
        &["6, row 6", "7, row 7", "8, row 8", "9, row 9", "10, row 10"],
    )?;

    // Enough beyond it to show that the query above is not a special case that happens to work.
    // One of each pipeline breaker, since those are the operators that materialize and are the ones
    // most likely to behave differently on a host with a different pointer width or hash seed.
    check(&db, "SELECT count(*) FROM t WHERE x > 5", &["5"])?;
    check(&db, "SELECT sum(x), min(x), max(x) FROM t", &["55, 1, 10"])?;
    check(&db, "SELECT x FROM t ORDER BY x DESC LIMIT 3", &["10", "9", "8"])?;
    check(
        &db,
        "SELECT x % 3 AS bucket, count(*) FROM t GROUP BY x % 3 ORDER BY bucket",
        &["0, 3", "1, 4", "2, 3"],
    )?;
    check(&db, "SELECT a.x FROM t AS a JOIN t AS b ON a.x = b.x WHERE a.x = 7", &["7"])?;
    check(&db, "SELECT 1 UNION SELECT 1", &["1"])?;

    println!();
    println!("the query in M0's exit criterion runs on this host and the answers are right");
    Ok(())
}

/// Runs one query and compares every row against what it should be.
///
/// The expected rows are written as text rather than as values, because this file is read by
/// somebody deciding whether a host passed and `6, row 6` is legible in a way that a nested vector
/// of enum variants is not. The comparison is on the rendered form for the same reason, and it is
/// exact: same rows, same order, same values.
fn check(db: &Database, sql: &str, expected: &[&str]) -> Result<(), String> {
    println!("{sql}");
    let result = db.query(sql).map_err(|e| format!("  {sql}\n  failed: {e}"))?;

    let mut actual = Vec::new();
    for row in result.rows() {
        let mut line = String::new();
        for (at, value) in row.iter().enumerate() {
            if at > 0 {
                line.push_str(", ");
            }
            let _ = write!(line, "{value}");
        }
        actual.push(line);
    }

    for line in &actual {
        println!("  {line}");
    }

    if actual.len() != expected.len() || actual.iter().zip(expected).any(|(a, b)| a != b) {
        return Err(format!(
            "{sql}\n  expected {} rows:\n    {}\n  got {} rows:\n    {}",
            expected.len(),
            expected.join("\n    "),
            actual.len(),
            actual.join("\n    ")
        ));
    }
    Ok(())
}
