//! Late materialisation over a Parquet file, from the SQL down to the rows that come back.
//!
//! `SELECT * FROM hits ORDER BY EventTime LIMIT 10` reads one column to decide which ten rows win
//! and a hundred and five columns of the ten that did. The pass rewrites that into a scan of the
//! ordering column and the row's ordinal, a top N over those two, and a fetch of the rest for the
//! ten rows left. What this file checks is that the rewritten plan is the shape it should be and
//! gives the rows the unrewritten one gives, because a rewrite that is fast and wrong is worse than
//! no rewrite at all.
//!
//! The fixture is `rudb-parquet`'s seven column file. Seven is under the width the pass asks for, so
//! the queries here name five of the columns twice. That is not what a real query looks like and it
//! is not meant to be. It puts the projection over the line with a file that is already in the tree,
//! and the plan the pass writes is the same plan either way.

use rudb::Database;
use rudb_common::Value;

/// The path of the fixture, as a SQL string literal.
fn fixture() -> String {
    format!("'{}/../rudb-parquet/testdata/mixed.parquet'", env!("CARGO_MANIFEST_DIR"))
}

/// Twelve columns, which is the seven the file has and five of them a second time.
const WIDE: &str = "a, b, s, d, flag, day, t, a AS a2, b AS b2, s AS s2, d AS d2, flag AS flag2";

/// Four keys, which taken together are different in all 4096 rows of the fixture, so a top N over
/// them has one answer and a test can compare two plans row by row.
const KEYS: &str = "b DESC, t ASC, a ASC, d ASC";

/// `SELECT <columns> FROM <fixture> ORDER BY <keys> LIMIT <count>`.
fn wide(columns: &str, keys: &str, count: usize) -> String {
    format!("SELECT {columns} FROM read_parquet({}) ORDER BY {keys} LIMIT {count}", fixture())
}

/// A database with the pass turned off by the name DuckDB gives it.
fn without_the_pass() -> Database {
    let database = Database::new();
    database
        .execute("SET disabled_optimizers = 'late_materialization'")
        .expect("the pass answers to that name");
    database
}

/// The rows of a query, as values.
fn rows(database: &Database, sql: &str) -> Vec<Vec<Value>> {
    let result = database.query(sql).unwrap_or_else(|error| panic!("{sql} failed: {error}"));
    result.rows().collect()
}

#[test]
fn a_wide_top_n_over_a_file_scans_the_ordering_column_and_fetches_the_rest() {
    let database = Database::new();
    let plan = database.plan(&wide(WIDE, "b DESC", 3)).expect("binds");
    assert!(plan.contains("Fetch args="), "{plan}");
    assert!(plan.contains("options=[file_row_number=TRUE::BOOLEAN]"), "{plan}");
    assert!(plan.contains("[b::BIGINT, file_row_number::BIGINT]"), "{plan}");
    let under = plan.split("TopN").nth(1).expect("the plan has a top N in it");
    for dropped in ["s::VARCHAR", "d::DOUBLE", "flag::BOOLEAN", "day::DATE", "t::TIMESTAMP"] {
        assert!(!under.contains(dropped), "{dropped} is read under the top N in {plan}");
    }
}

#[test]
fn the_rewritten_plan_answers_what_the_plan_without_it_answers() {
    let sql = wide(WIDE, KEYS, 5);
    let deferred = rows(&Database::new(), &sql);
    let plain = rows(&without_the_pass(), &sql);
    assert_eq!(deferred.len(), 5);
    assert_eq!(deferred, plain);
}

#[test]
fn an_offset_past_the_first_rows_is_the_same_rows_either_way() {
    // The pass has to fetch the rows the top N kept after the offset was applied and not the ones
    // it threw away, which is the part of the rewrite an offset is there to catch.
    let sql = format!("{} OFFSET 7", wide(WIDE, KEYS, 4));
    let deferred = rows(&Database::new(), &sql);
    let plain = rows(&without_the_pass(), &sql);
    assert_eq!(deferred.len(), 4);
    assert_eq!(deferred, plain);
}

#[test]
fn the_fetched_rows_keep_the_order_the_top_n_put_them_in() {
    // The reader walks the file forwards, so the fetch reads the winners in file order and has to
    // put them back. Ordering by a column that runs the other way from the file is the check.
    let database = Database::new();
    let result = database.query(&wide(WIDE, "b DESC", 6)).expect("runs");
    let held: Vec<i64> = result
        .column(1)
        .map(|value| match value {
            Value::BigInt(number) => number,
            other => panic!("a BIGINT column produced {other:?}"),
        })
        .collect();
    let mut sorted = held.clone();
    sorted.sort_by(|left, right| right.cmp(left));
    assert_eq!(held, sorted, "the rows came back out of order");
}

#[test]
fn the_scan_under_the_top_n_reads_the_ordering_column_and_no_others() {
    // What is compared is the scan and not the whole query, because the fixture is 4096 rows in two
    // row groups and a fetch of five rows out of it reads a whole row group of every column back.
    // The saving is the part of the file the top N no longer walks, and on a file with one row group
    // it is not a saving at all. On ClickBench at ten million rows the scan is the whole cost.
    let sql = wide(WIDE, "day ASC", 5);
    let read = |database: &Database| {
        let result = database.query(&sql).expect("runs");
        let metrics = result.metrics().expect("a query that ran has metrics");
        metrics
            .operators
            .iter()
            .find(|operator| operator.kind == "FileScan")
            .expect("the query contains a file scan")
            .bytes_read
    };
    let deferred = read(&Database::new());
    let plain = read(&without_the_pass());
    assert!(deferred * 4 < plain, "the narrowed scan read {deferred} bytes against {plain}");
}

#[test]
fn a_projection_that_is_not_wide_is_left_alone() {
    // Three columns through a top N is three columns either way. The fetch would be a second read
    // of the file to save carrying two columns of five rows, which is not a trade worth making.
    let database = Database::new();
    let plan = database.plan(&wide("a, b, s", "b DESC", 5)).expect("binds");
    assert!(!plan.contains("Fetch"), "{plan}");
}

#[test]
fn a_limit_that_is_not_small_is_left_alone() {
    // Fetching by ordinal is a seek for each row. Once the limit is most of the file, reading the
    // file once and carrying the rows is the cheaper of the two.
    let database = Database::new();
    let plan = database.plan(&wide(WIDE, "b DESC", 4096)).expect("binds");
    assert!(!plan.contains("Fetch"), "{plan}");
}

#[test]
fn a_computed_ordering_key_is_replayed_after_the_fetch() {
    let database = Database::new();
    let sql = wide(WIDE, "b + 1 DESC", 5);
    let plan = database.plan(&sql).expect("binds");
    assert!(plan.contains("Fetch"), "{plan}");
    assert_eq!(rows(&database, &sql), rows(&without_the_pass(), &sql));
}

#[test]
fn a_file_that_already_counts_its_rows_is_left_alone() {
    // The scan would end up with two columns of that name and the fetch would have no way to say
    // which of them it reads from.
    let database = Database::new();
    let call = format!("read_parquet({}, file_row_number=True)", fixture());
    let sql = format!("SELECT {WIDE}, file_row_number FROM {call} ORDER BY b DESC LIMIT 5");
    let plan = database.plan(&sql).expect("binds");
    assert!(!plan.contains("Fetch"), "{plan}");
}
