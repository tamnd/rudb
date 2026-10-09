//! A filter on a table small enough to run it over is counted rather than estimated.
//!
//! JOB's selective filters are mostly on dimension tables of a few hundred thousand rows at most,
//! and the sample the planner otherwise reads holds about fifteen thousand of them. A value one row
//! holds is in no sample, so `k.keyword = 'character-name-in-title'` was put at four rows. Run over
//! every row, it is one, and `EXPLAIN` says the count is exact and that the filter was evaluated.

use rudb::Database;

/// The plan of `sql` over a native file holding a table of ten thousand distinct words, a hundred
/// of which start with `x`.
fn explained(tag: &str, sql: &str) -> String {
    let path =
        std::env::temp_dir().join(format!("rudb-evaluated-{tag}-{}.rudb", std::process::id()));
    let _ = std::fs::remove_file(&path);
    let name = path.to_str().expect("a UTF-8 temporary path");
    {
        let writing = Database::open(name).expect("a file name starts a native database");
        for sql in [
            "CREATE TABLE word(id INTEGER, word VARCHAR)",
            "INSERT INTO word SELECT i, CASE WHEN i % 100 = 0 THEN 'x' ELSE 'w' END || i::VARCHAR \
             FROM range(10000) r(i)",
        ] {
            writing.execute(sql).expect("the table is made");
        }
        writing.execute("CHECKPOINT").expect("the table is committed");
    }
    let file = Database::open(name).expect("the written file opens again");
    let explained: String = file
        .query(&format!("EXPLAIN {sql}"))
        .expect("the plan is explained")
        .rows()
        .flatten()
        .map(|value| format!("{value}\n"))
        .collect();
    drop(file);
    let _ = std::fs::remove_file(&path);
    explained
}

/// The line of the plan the filter is on.
fn filter_line(explained: &str) -> &str {
    explained
        .lines()
        .find(|line| line.contains("Filter "))
        .unwrap_or_else(|| panic!("no filter in {explained}"))
}

#[test]
fn an_equality_one_row_holds_is_counted_as_one_row() {
    let text = explained("one", "SELECT min(id) FROM word WHERE word = 'w17'");
    let line = filter_line(&text);
    assert!(line.contains("[1 rows exact from evaluated]"), "{text}");
}

#[test]
fn an_in_list_and_a_pattern_are_counted_over_every_row() {
    let text = explained("in", "SELECT min(id) FROM word WHERE word IN ('w17', 'w18', 'q')");
    let line = filter_line(&text);
    assert!(line.contains("[2 rows exact from evaluated]"), "{text}");
    let text = explained("like", "SELECT min(id) FROM word WHERE word LIKE 'x%'");
    let line = filter_line(&text);
    assert!(line.contains("[100 rows exact from evaluated]"), "{text}");
}

#[test]
fn a_filter_of_two_conditions_is_left_to_the_estimate() {
    let text = explained("two", "SELECT min(id) FROM word WHERE word LIKE 'x%' AND id > 5000");
    let line = filter_line(&text);
    assert!(!line.contains("evaluated"), "{text}");
}
