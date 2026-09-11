//! Every ClickBench query, unmodified, put through the front end.
//!
//! The benchmark rule is that the SQL is the SQL. ClickBench publishes one file of queries per
//! engine and the DuckDB file is the one rudb has to answer, character for character, because a
//! number produced by a rewritten query is a number about a different query. So this test takes
//! that file, creates the `hits` table from the official DDL, and asks the front end to plan all
//! forty three. It never executes anything, because execution needs the data and the data needs the
//! Parquet reader, and the point here is to find out which of the two halves is actually in the
//! way.
//!
//! What fails is listed below with the reason. The list is asserted to be exactly the set that
//! fails, not a subset, so closing a gap without deleting its line fails the test as loudly as
//! opening a new one does. That is deliberate: the number in this list is the honest measure of how
//! far the front end is from the compatibility claim, and it is only worth anything if it cannot
//! drift.
//!
//! The list is now empty, which took the count from thirty four to forty three over five changes.
//! Holding it there is what the equality in the assertion is for.

use rudb::Database;

/// The queries, as they come from the ClickBench repository by way of the suite in
/// `tamnd/rudb-bench`.
const SQL: &str = include_str!("../testdata/clickbench.sql");

/// The queries the front end cannot plan yet, and what stops each one.
///
/// Every entry is a missing piece of DuckDB rather than a difference of opinion about SQL, so every
/// entry is a bug with a fix rather than a note about dialects.
///
/// The list is empty. All forty three plan, against the official DDL, with the SQL unmodified. That
/// is the front end done as a measure, and it is only the front end: planning a query is not
/// answering it, and what the numbers in `tamnd/rudb-bench` need next is the Parquet reader wired to
/// a table function so there is data under these plans.
const GAPS: &[(&str, &str)] = &[];

/// Splits the file into the statements it holds, each with the name of the comment above it.
///
/// The DDL comes back under the name `hits`, since it is the one statement in the file with no
/// query number over it.
fn statements() -> Vec<(String, String)> {
    let mut found = Vec::new();
    let mut name = String::from("hits");
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
            found.push((name.clone(), statement.to_string()));
            sql.clear();
        }
    }
    assert!(sql.is_empty(), "a statement in the file does not end with a semicolon");
    found
}

/// A database with `hits` in it and nothing in `hits`.
fn with_hits() -> Database {
    let database = Database::new();
    let (name, ddl) = statements().remove(0);
    assert_eq!(name, "hits", "the first statement in the file is not the DDL");
    database.execute(&ddl).expect("the official hits DDL");
    database
}

#[test]
fn the_file_holds_the_whole_benchmark() {
    let found = statements();
    assert_eq!(found.len(), 44, "the DDL and forty three queries");
    let names: Vec<&str> = found[1..].iter().map(|(name, _)| name.as_str()).collect();
    let wanted: Vec<String> = (1..=43).map(|at| format!("q{at}")).collect();
    assert_eq!(names, wanted, "the queries are not q1 to q43 in order");
}

#[test]
fn the_official_ddl_creates_the_table() {
    let database = with_hits();
    assert_eq!(database.table_len("hits").expect("hits exists"), 0);
}

#[test]
fn every_query_plans_or_is_a_known_gap() {
    let database = with_hits();
    let mut failed = Vec::new();
    for (name, sql) in statements().into_iter().skip(1) {
        if let Err(error) = database.plan(&sql) {
            failed.push((name, error.message().to_string()));
        }
    }
    let names: Vec<&str> = failed.iter().map(|(name, _)| name.as_str()).collect();
    let known: Vec<&str> = GAPS.iter().map(|(name, _)| *name).collect();
    assert_eq!(
        names,
        known,
        "the queries that do not plan are not the ones listed as gaps, which failed like this: {}",
        failed
            .iter()
            .map(|(name, why)| format!("{name}: {why}"))
            .collect::<Vec<String>>()
            .join(", ")
    );
}
