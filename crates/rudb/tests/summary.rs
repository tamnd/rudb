//! Aggregates over a whole table that the storage format answers out of its directory.
//!
//! Every one of these has the same shape: the same question is asked of a table held in memory and
//! of the same table written to a file, and the two have to agree. The file is allowed to be faster
//! and it is not allowed to be different, so the in-memory answer is the oracle and the test is
//! whether owning the format bought speed or bought a wrong answer.
//!
//! The one that matters most is the nullable column. A null row is written as the code for the
//! empty string, so the dictionary of a nullable column can hold an empty string that no row of it
//! has, and a file that counted its codes or read the first of them in sorted order would report one
//! distinct value too many and an empty string as the minimum. That is not a rounding error, it is a
//! wrong answer to `COUNT(DISTINCT)`, and it is why a column with a null in it is left alone.
//!
//! The numbers come from somewhere else. Each stripe writes down the two ends of each column and
//! the total of it when the column is integers, so a whole table `MIN`, `MAX`, `SUM` and `AVG` is
//! the stripes put together. Floats and decimals are left out on purpose, and the test for that is
//! in `rudb-storage` rather than here, because the format does not store either type yet and a
//! test that cannot create the table is a test that proves nothing.

use rudb::Database;
use rudb_common::Value;

/// The same rows in memory and in a file, with the file's answer checked against the memory one.
struct Pair {
    memory: Database,
    file: Database,
    path: std::path::PathBuf,
}

impl Pair {
    fn new(tag: &str, select: &str) -> Self {
        let path =
            std::env::temp_dir().join(format!("rudb-summary-{tag}-{}.rudb", std::process::id()));
        let _ = std::fs::remove_file(&path);
        let create = format!("CREATE TABLE t AS {select}");
        let memory = Database::new();
        memory.execute(&create).expect("the memory table is created");
        let name = path.to_str().expect("a UTF-8 temporary path");
        // Written by one database and read by another, because that is the only way a table is
        // actually backed by the file. The one that wrote the rows is still holding them in memory
        // afterwards, so asking it anything would be asking the wrong side of this test.
        {
            let writing = Database::open(name).expect("a file name starts a native database");
            writing.execute(&create).expect("the file table is created");
            writing.execute("CHECKPOINT").expect("the file table is committed");
        }
        let file = Database::open(name).expect("the written file opens again");
        Self { memory, file, path }
    }

    /// Asserts the file agrees with memory, and returns what they both said.
    fn agree(&self, query: &str) -> Value {
        let wanted = self.memory.value(query).expect("the memory table answers");
        let got = self.file.value(query).expect("the file answers");
        assert_eq!(got, wanted, "the file and memory disagree about {query}");
        got
    }

    /// Whether the file answered this without reading any rows.
    ///
    /// Read off the operator the aggregate became rather than off the plan text, because the plan
    /// text cannot tell the two apart. A scan is folded into the operator above it either way, so
    /// the `Get` line says the same thing whether the rows were read or never looked at.
    fn summarised(&self, query: &str) -> bool {
        let result = self.file.query(query).expect("the query ran");
        let metrics = result.metrics().expect("the query was measured");
        metrics
            .operators
            .iter()
            .any(|operator| operator.detail.as_deref() == Some("native summary"))
    }
}

impl Drop for Pair {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.path);
    }
}

#[test]
fn counting_a_whole_table_reads_the_row_count_the_file_wrote_down() {
    let pair = Pair::new("count", "SELECT i AS n, 'v' || (i % 13) AS s FROM range(5000) r(i)");
    assert_eq!(pair.agree("SELECT COUNT(*) FROM t"), Value::BigInt(5000));
    assert!(pair.summarised("SELECT COUNT(*) FROM t"), "the rows were read anyway");
    assert_eq!(pair.agree("SELECT COUNT(s) FROM t"), Value::BigInt(5000));
    assert!(pair.summarised("SELECT COUNT(s) FROM t"), "the rows were read anyway");
}

#[test]
fn counting_the_distinct_values_of_a_string_column_reads_the_size_of_its_dictionary() {
    let pair = Pair::new("distinct", "SELECT 'v' || (i % 13) AS s FROM range(5000) r(i)");
    assert_eq!(pair.agree("SELECT COUNT(DISTINCT s) FROM t"), Value::BigInt(13));
    assert!(pair.summarised("SELECT COUNT(DISTINCT s) FROM t"), "the rows were grouped anyway");
}

#[test]
fn the_extremes_of_a_string_column_are_the_two_ends_of_the_order_beside_its_values() {
    let pair = Pair::new("extremes", "SELECT 'v' || (i % 13) AS s FROM range(5000) r(i)");
    // Sorted by bytes rather than by the number in them, so v9 is the largest of thirteen values
    // that go up to v12. Both sides do that, which is the point of asking both.
    assert_eq!(pair.agree("SELECT MIN(s) FROM t"), Value::Varchar("v0".to_owned()));
    assert_eq!(pair.agree("SELECT MAX(s) FROM t"), Value::Varchar("v9".to_owned()));
    assert!(pair.summarised("SELECT MIN(s), MAX(s) FROM t"), "the rows were read anyway");
}

#[test]
fn a_column_with_a_null_in_it_is_counted_the_ordinary_way() {
    let pair = Pair::new(
        "nulls",
        "SELECT CASE WHEN i % 11 = 0 THEN NULL ELSE 'v' || (i % 13) END AS s FROM range(5000) r(i)",
    );
    // Thirteen values, and the empty string the nulls were written as is not one of them.
    assert_eq!(pair.agree("SELECT COUNT(DISTINCT s) FROM t"), Value::BigInt(13));
    // The extremes are a different matter. They do not come from the dictionary here, they come
    // from the stripe ranges, and those were walked a row at a time with the nulls skipped, so the
    // empty string the nulls were written as never got near them.
    assert_eq!(pair.agree("SELECT MIN(s) FROM t"), Value::Varchar("v0".to_owned()));
    assert!(pair.summarised("SELECT MIN(s) FROM t"), "the rows were read anyway");
    // The count of the column is still the file's business, because a null count is written down
    // exactly rather than as a bound that is allowed to be wide.
    assert_eq!(pair.agree("SELECT COUNT(s) FROM t"), Value::BigInt(4545));
    assert!(pair.summarised("SELECT COUNT(s) FROM t"), "the rows were read anyway");
}

#[test]
fn a_filter_or_a_grouping_sends_the_query_back_to_the_rows() {
    let pair =
        Pair::new("filtered", "SELECT i % 7 AS n, 'v' || (i % 13) AS s FROM range(5000) r(i)");
    assert_eq!(pair.agree("SELECT COUNT(*) FROM t WHERE n = 1"), Value::BigInt(715));
    assert!(!pair.summarised("SELECT COUNT(*) FROM t WHERE n = 1"), "a filter was ignored");
    assert_eq!(pair.agree("SELECT COUNT(*) FROM t WHERE s <> 'v0'"), Value::BigInt(4615));
    assert!(!pair.summarised("SELECT COUNT(*) FROM t WHERE s <> 'v0'"), "a filter was ignored");
    // A grouped count is a different question and the synopsis answers that one, not this.
    assert_eq!(pair.agree("SELECT COUNT(*) FROM t GROUP BY n LIMIT 1"), Value::BigInt(715));
}

#[test]
fn a_numeric_column_has_no_dictionary_so_its_distinct_values_are_still_counted() {
    let pair = Pair::new("numeric", "SELECT i % 7 AS n FROM range(5000) r(i)");
    assert_eq!(pair.agree("SELECT COUNT(DISTINCT n) FROM t"), Value::BigInt(7));
    assert!(!pair.summarised("SELECT COUNT(DISTINCT n) FROM t"), "a numeric column was summarised");
}

#[test]
fn the_ends_and_the_total_of_an_integer_column_are_its_stripe_ranges_added_up() {
    let pair = Pair::new("integers", "SELECT i % 7 AS n FROM range(5000) r(i)");
    assert_eq!(pair.agree("SELECT MIN(n) FROM t"), Value::BigInt(0));
    assert_eq!(pair.agree("SELECT MAX(n) FROM t"), Value::BigInt(6));
    assert_eq!(pair.agree("SELECT SUM(n) FROM t"), Value::HugeInt(14995));
    assert_eq!(pair.agree("SELECT AVG(n) FROM t"), Value::Double(2.999));
    assert!(pair.summarised("SELECT MIN(n), MAX(n), SUM(n), AVG(n) FROM t"), "the rows were read");
}

#[test]
fn an_integer_column_with_nulls_in_it_is_still_answered_because_the_null_count_is_exact() {
    let pair = Pair::new(
        "nullints",
        "SELECT CASE WHEN i % 11 = 0 THEN NULL ELSE i % 7 END AS n FROM range(5000) r(i)",
    );
    assert_eq!(pair.agree("SELECT MIN(n) FROM t"), Value::BigInt(0));
    assert_eq!(pair.agree("SELECT COUNT(n) FROM t"), Value::BigInt(4545));
    pair.agree("SELECT SUM(n) FROM t");
    pair.agree("SELECT AVG(n) FROM t");
    assert!(pair.summarised("SELECT SUM(n), AVG(n) FROM t"), "the rows were read anyway");
}

#[test]
fn an_integer_column_that_is_all_nulls_answers_what_an_aggregate_over_nothing_answers() {
    let pair = Pair::new("allnull", "SELECT CAST(NULL AS BIGINT) AS n FROM range(5000) r(i)");
    assert_eq!(pair.agree("SELECT SUM(n) FROM t"), Value::Null);
    assert_eq!(pair.agree("SELECT AVG(n) FROM t"), Value::Null);
    assert!(pair.summarised("SELECT SUM(n), AVG(n) FROM t"), "the rows were read anyway");
}

#[test]
fn an_empty_table_still_answers_what_an_aggregate_over_nothing_answers() {
    let pair = Pair::new("empty", "SELECT 'v' AS s FROM range(0) r(i)");
    assert_eq!(pair.agree("SELECT COUNT(*) FROM t"), Value::BigInt(0));
    assert_eq!(pair.agree("SELECT COUNT(DISTINCT s) FROM t"), Value::BigInt(0));
    assert_eq!(pair.agree("SELECT MIN(s) FROM t"), Value::Null);
}
