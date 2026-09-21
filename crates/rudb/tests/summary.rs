//! Aggregates over a whole table that are answered out of statistics rather than out of the rows.
//!
//! Every one of these has the same shape: the same question is asked of a table held in memory and
//! of the same table written to a file, and the two have to agree. The file is allowed to be faster
//! and it is not allowed to be different, so the test is whether owning the format bought speed or
//! bought a wrong answer.
//!
//! The in-memory side used to be the oracle here, because it read every row and the file read a
//! directory. It is not any more. A table in memory keeps a zone map per chunk and now answers a
//! `COUNT`, a `MIN`, a `MAX`, a `SUM` and an `AVG` over a whole table out of that, so for those five
//! this compares two statistics paths against each other and against a number written out in the
//! test. Where a number is asserted below it is the answer worked out by hand, and that is the
//! oracle. What memory still reads the rows for is everything that needs a persisted synopsis, which
//! is the distinct count and the frequencies, and those are the cases where the two sides really do
//! take different routes to the same answer.
//!
//! The one that matters most is the nullable column. A null row is written as the code for the
//! empty string, so the dictionary of a nullable column can hold an empty string that no row of it
//! has, and a file that counted its codes or read the first of them in sorted order would report one
//! distinct value too many and an empty string as the minimum. That is not a rounding error, it is a
//! wrong answer to `COUNT(DISTINCT)`. The count is now settled by the writer, which knows how many
//! codes a row of the column actually holds, so a null no longer stops it. The minimum is still
//! stopped by it, because which entry of the sorted order is the first one a row holds is not
//! something the directory says.
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
        self.answered(&self.file, query, "stored summary")
    }

    /// Whether the table in memory answered this without reading any rows, out of its zone maps.
    fn in_memory(&self, query: &str) -> bool {
        self.answered(&self.memory, query, "stored summary")
    }

    /// Whether the file answered this at plan time, with no aggregate left to run at all.
    ///
    /// A step past [`Pair::summarised`] rather than a different route to it. Both read the answer
    /// off the directory and neither touches a row, but the summary is an operator that produces
    /// one row and this is the optimizer folding the aggregate into a constant and deleting the
    /// scan under it, so there is nothing left in the pipeline to count.
    fn folded(&self, query: &str) -> bool {
        let result = self.file.query(query).expect("the query ran");
        let metrics = result.metrics().expect("the query was measured");
        metrics.operators.iter().all(|operator| operator.kind != "Aggregate")
    }

    /// Whether the file built the groups of this out of the synopsis rather than out of the rows.
    ///
    /// A different operator from the one above, because a grouped count is answered by reading a
    /// list of groups out of the file and an ungrouped one by adding a few numbers up, so the two
    /// say different things about themselves.
    fn grouped(&self, query: &str) -> bool {
        self.answered(&self.file, query, "native frequencies")
    }

    /// Whether any operator of this query on this database is the one named.
    fn answered(&self, db: &Database, query: &str, detail: &str) -> bool {
        let result = db.query(query).expect("the query ran");
        let metrics = result.metrics().expect("the query was measured");
        metrics.operators.iter().any(|operator| operator.detail.as_deref() == Some(detail))
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
    // Folded rather than summarised. The file now hands its stripe bounds to the planner, so the
    // aggregate is gone by the time anything is built and the answer is a row of constants.
    assert!(pair.folded("SELECT MIN(s), MAX(s) FROM t"), "the rows were read anyway");
}

#[test]
fn a_column_with_a_null_in_it_is_counted_out_of_the_file_as_well() {
    let pair = Pair::new(
        "nulls",
        "SELECT CASE WHEN i % 11 = 0 THEN NULL ELSE 'v' || (i % 13) END AS s FROM range(5000) r(i)",
    );
    // Thirteen values, and the empty string the nulls were written as is not one of them. The
    // dictionary has fourteen entries here and the file says thirteen, because the writer counted
    // the rows that hold each code and the empty string's code is held by none of them.
    assert_eq!(pair.agree("SELECT COUNT(DISTINCT s) FROM t"), Value::BigInt(13));
    assert!(pair.summarised("SELECT COUNT(DISTINCT s) FROM t"), "the rows were grouped anyway");
    // Counting the groups is a different number, because the nulls are a group of their own and are
    // not a distinct value. Fourteen groups over thirteen values.
    assert_eq!(
        pair.agree("SELECT COUNT(*) FROM (SELECT s FROM t GROUP BY s) g"),
        Value::BigInt(14)
    );
    // The extremes are a different matter. They do not come from the dictionary here, they come
    // from the stripe ranges, and those were walked a row at a time with the nulls skipped, so the
    // empty string the nulls were written as never got near them.
    assert_eq!(pair.agree("SELECT MIN(s) FROM t"), Value::Varchar("v0".to_owned()));
    assert!(pair.folded("SELECT MIN(s) FROM t"), "the rows were read anyway");
    // The count of the column is still the file's business, because a null count is written down
    // exactly rather than as a bound that is allowed to be wide.
    assert_eq!(pair.agree("SELECT COUNT(s) FROM t"), Value::BigInt(4545));
    assert!(pair.summarised("SELECT COUNT(s) FROM t"), "the rows were read anyway");
}

#[test]
fn a_column_holding_both_nulls_and_empty_strings_counts_the_empty_string_once() {
    // The case the null placeholder makes awkward. Rows divisible by eleven are null and rows
    // divisible by seven are a real empty string, so the empty string is a value of this column and
    // has to be counted, and the two of them share a dictionary entry. Fourteen distinct values.
    let pair = Pair::new(
        "sharednull",
        "SELECT CASE WHEN i % 11 = 0 THEN NULL WHEN i % 7 = 0 THEN '' \
         ELSE 'v' || (i % 13) END AS s FROM range(5000) r(i)",
    );
    assert_eq!(pair.agree("SELECT COUNT(DISTINCT s) FROM t"), Value::BigInt(14));
    assert!(pair.summarised("SELECT COUNT(DISTINCT s) FROM t"), "the rows were grouped anyway");
}

#[test]
fn a_column_that_is_nothing_but_nulls_has_no_distinct_values_at_all() {
    let pair =
        Pair::new("allnullstrings", "SELECT CAST(NULL AS VARCHAR) AS s FROM range(500) r(i)");
    assert_eq!(pair.agree("SELECT COUNT(DISTINCT s) FROM t"), Value::BigInt(0));
    assert!(pair.summarised("SELECT COUNT(DISTINCT s) FROM t"), "the rows were grouped anyway");
}

#[test]
fn one_comparison_against_a_constant_is_counted_out_of_the_frequency_synopsis() {
    let pair =
        Pair::new("filtered", "SELECT i % 7 AS n, 'v' || (i % 13) AS s FROM range(5000) r(i)");
    // Seven values and thirteen values, both far inside the budget the synopsis keeps, so neither
    // column ever had to drop one and both lists are the whole column with an exact count.
    assert_eq!(pair.agree("SELECT COUNT(*) FROM t WHERE n = 1"), Value::BigInt(715));
    assert!(pair.summarised("SELECT COUNT(*) FROM t WHERE n = 1"), "the rows were read anyway");
    assert_eq!(pair.agree("SELECT COUNT(*) FROM t WHERE n <> 1"), Value::BigInt(4285));
    assert!(pair.summarised("SELECT COUNT(*) FROM t WHERE n <> 1"), "the rows were read anyway");
    assert_eq!(pair.agree("SELECT COUNT(*) FROM t WHERE s <> 'v0'"), Value::BigInt(4615));
    assert!(pair.summarised("SELECT COUNT(*) FROM t WHERE s <> 'v0'"), "the rows were read anyway");
    assert_eq!(pair.agree("SELECT COUNT(*) FROM t WHERE s = 'v0'"), Value::BigInt(385));
    assert!(pair.summarised("SELECT COUNT(*) FROM t WHERE s = 'v0'"), "the rows were read anyway");
    // Written the other way round is the same question and neither side depends on the other.
    assert_eq!(pair.agree("SELECT COUNT(*) FROM t WHERE 1 = n"), Value::BigInt(715));
    assert!(pair.summarised("SELECT COUNT(*) FROM t WHERE 1 = n"), "the rows were read anyway");
    assert_eq!(pair.agree("SELECT COUNT(*) FROM t WHERE 'v0' <> s"), Value::BigInt(4615));
    assert!(pair.summarised("SELECT COUNT(*) FROM t WHERE 'v0' <> s"), "the rows were read anyway");
    // A value the column does not have is the same walk and the answer is none of the rows, which
    // is worth asking because it is the one case where the count comes out of an empty sum.
    assert_eq!(pair.agree("SELECT COUNT(*) FROM t WHERE n = 99"), Value::BigInt(0));
    assert!(pair.summarised("SELECT COUNT(*) FROM t WHERE n = 99"), "the rows were read anyway");
    assert_eq!(pair.agree("SELECT COUNT(*) FROM t WHERE n <> 99"), Value::BigInt(5000));
    assert!(pair.summarised("SELECT COUNT(*) FROM t WHERE n <> 99"), "the rows were read anyway");
    // A grouped count is a different question and the synopsis answers that one too.
    assert_eq!(pair.agree("SELECT COUNT(*) FROM t GROUP BY n LIMIT 1"), Value::BigInt(715));
}

#[test]
fn a_filter_over_a_column_with_nulls_leaves_the_nulls_out_of_both_comparisons() {
    let pair = Pair::new(
        "filternulls",
        "SELECT CASE WHEN i % 11 = 0 THEN NULL ELSE i % 7 END AS n, \
         CASE WHEN i % 11 = 0 THEN NULL ELSE 'v' || (i % 13) END AS s FROM range(5000) r(i)",
    );
    // The synopsis counts a null as a value of its own rather than skipping it, so a count over its
    // entries that just compared would hand the 455 null rows to `<>` and SQL hands them to neither.
    assert_eq!(pair.agree("SELECT COUNT(*) FROM t WHERE n = 1"), Value::BigInt(650));
    assert!(pair.summarised("SELECT COUNT(*) FROM t WHERE n = 1"), "the rows were read anyway");
    assert_eq!(pair.agree("SELECT COUNT(*) FROM t WHERE n <> 1"), Value::BigInt(3895));
    assert!(pair.summarised("SELECT COUNT(*) FROM t WHERE n <> 1"), "the rows were read anyway");
    assert_eq!(pair.agree("SELECT COUNT(*) FROM t WHERE s = 'v0'"), Value::BigInt(350));
    assert!(pair.summarised("SELECT COUNT(*) FROM t WHERE s = 'v0'"), "the rows were read anyway");
    assert_eq!(pair.agree("SELECT COUNT(*) FROM t WHERE s <> 'v0'"), Value::BigInt(4195));
    assert!(pair.summarised("SELECT COUNT(*) FROM t WHERE s <> 'v0'"), "the rows were read anyway");
    // The two sides of that add up to the rows that are not null rather than to the whole table.
    assert_eq!(pair.agree("SELECT COUNT(n) FROM t"), Value::BigInt(4545));
}

#[test]
fn a_filter_the_synopsis_cannot_decide_sends_the_query_back_to_the_rows() {
    let pair = Pair::new("filterback", "SELECT i % 7 AS n, i AS wide FROM range(5000) r(i)");
    // An ordering comparison is a different question from picking entries out of a list, and the
    // shortcut turns it away rather than guessing at it.
    assert_eq!(pair.agree("SELECT COUNT(*) FROM t WHERE n > 1"), Value::BigInt(3570));
    assert!(!pair.summarised("SELECT COUNT(*) FROM t WHERE n > 1"), "a filter was ignored");
    // So is more than one of them.
    assert_eq!(pair.agree("SELECT COUNT(*) FROM t WHERE n = 1 AND wide < 100"), Value::BigInt(15));
    assert!(
        !pair.summarised("SELECT COUNT(*) FROM t WHERE n = 1 AND wide < 100"),
        "filter ignored"
    );
    // Five thousand distinct values overflow the budget, so the file kept the leading ones and a
    // bound on what it dropped, and a bound cannot be counted with.
    assert_eq!(pair.agree("SELECT COUNT(*) FROM t WHERE wide = 1"), Value::BigInt(1));
    assert!(
        !pair.summarised("SELECT COUNT(*) FROM t WHERE wide = 1"),
        "a partial list was counted"
    );
    assert_eq!(pair.agree("SELECT COUNT(*) FROM t WHERE wide <> 1"), Value::BigInt(4999));
    assert!(
        !pair.summarised("SELECT COUNT(*) FROM t WHERE wide <> 1"),
        "a partial list was counted"
    );
    // A null constant compares unknown against every row whatever the column holds, so the answer
    // is none of them, and that is the operator's own rule rather than something worth a shape.
    assert_eq!(pair.agree("SELECT COUNT(*) FROM t WHERE n = NULL"), Value::BigInt(0));
}

#[test]
fn a_grouped_count_over_a_complete_synopsis_is_read_out_of_it_filter_and_all() {
    let pair = Pair::new("grouped", "SELECT i % 7 AS n, i AS wide FROM range(5000) r(i)");
    // Seven groups and the file holds all seven with an exact count, so there is nothing to build.
    assert_eq!(
        pair.agree("SELECT COUNT(*) FROM t GROUP BY n ORDER BY 1 DESC LIMIT 1"),
        Value::BigInt(715)
    );
    assert!(pair.grouped("SELECT n, COUNT(*) FROM t GROUP BY n"), "the rows were grouped anyway");
    // A filter over the column being grouped only decides which groups survive, so it rides along.
    assert!(
        pair.grouped("SELECT n, COUNT(*) FROM t WHERE n <> 1 GROUP BY n ORDER BY 2 DESC"),
        "the rows were grouped anyway"
    );
    assert_eq!(
        pair.agree("SELECT COUNT(*) FROM (SELECT n, COUNT(*) FROM t WHERE n <> 1 GROUP BY n) g"),
        Value::BigInt(6),
    );
    // A filter over some other column decides rows inside a group instead, and the synopsis of the
    // grouped column says nothing about which of its rows those are.
    assert!(
        !pair.grouped("SELECT n, COUNT(*) FROM t WHERE wide = 3 GROUP BY n"),
        "a filter over another column was ignored"
    );
    assert_eq!(
        pair.agree("SELECT COUNT(*) FROM (SELECT n, COUNT(*) FROM t WHERE wide = 3 GROUP BY n) g"),
        Value::BigInt(1),
    );
    // And a column whose values overflow the budget has no complete list to group out of.
    assert!(
        !pair.grouped("SELECT wide, COUNT(*) FROM t GROUP BY wide"),
        "a partial list was grouped out of"
    );
}

#[test]
fn a_grouped_count_over_a_complete_synopsis_keeps_the_null_group_the_rows_would() {
    let pair = Pair::new(
        "groupnulls",
        "SELECT CASE WHEN i % 11 = 0 THEN NULL ELSE i % 7 END AS n FROM range(5000) r(i)",
    );
    // Eight groups, because a grouping puts the nulls in one of their own and the synopsis counts
    // them as a value of their own, which is the pair of facts that makes these agree.
    assert_eq!(
        pair.agree("SELECT COUNT(*) FROM (SELECT n, COUNT(*) FROM t GROUP BY n) g"),
        Value::BigInt(8),
    );
    assert!(pair.grouped("SELECT n, COUNT(*) FROM t GROUP BY n"), "the rows were grouped anyway");
    // With a filter on the same column the null group goes, because the comparison keeps neither
    // side of it, and that leaves the six groups the seven minus the filtered one comes to.
    assert_eq!(
        pair.agree("SELECT COUNT(*) FROM (SELECT n, COUNT(*) FROM t WHERE n <> 1 GROUP BY n) g"),
        Value::BigInt(6),
    );
    assert!(
        pair.grouped("SELECT n, COUNT(*) FROM t WHERE n <> 1 GROUP BY n"),
        "the rows were grouped anyway"
    );
    assert_eq!(
        pair.agree("SELECT SUM(c) FROM (SELECT COUNT(*) AS c FROM t WHERE n <> 1 GROUP BY n) g"),
        Value::HugeInt(3895)
    );
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
    // Worked out here rather than taken from whichever side answered first, because both sides
    // answer this one out of statistics now and a test where the two paths only have to agree with
    // each other would pass if they were wrong in the same way. The 4545 rows that are not null hold
    // `i % 7` and add up to 13630.
    let (mut total, mut rows) = (0_i64, 0_i64);
    for i in 0..5000 {
        if i % 11 != 0 {
            total += i % 7;
            rows += 1;
        }
    }
    assert_eq!((total, rows), (13630, 4545), "the rows this table was built from");
    assert_eq!(pair.agree("SELECT SUM(n) FROM t"), Value::HugeInt(i128::from(total)));
    #[expect(clippy::cast_precision_loss, reason = "13630 over 4545 is exact in a double")]
    let average = total as f64 / rows as f64;
    assert_eq!(pair.agree("SELECT AVG(n) FROM t"), Value::Double(average));
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

#[test]
fn a_table_in_memory_answers_out_of_its_zone_maps_without_reading_its_rows() {
    let pair = Pair::new(
        "memzones",
        "SELECT i % 7 AS n, CASE WHEN i % 11 = 0 THEN NULL ELSE i % 5 END AS m FROM range(5000) r(i)",
    );
    // The five a zone map holds: a row count, an exact null count, the two ends and a total.
    assert!(pair.in_memory("SELECT COUNT(*) FROM t"), "the rows were counted");
    assert!(pair.in_memory("SELECT COUNT(m) FROM t"), "the nulls were counted");
    assert!(pair.in_memory("SELECT MIN(n), MAX(n) FROM t"), "the ends were walked");
    assert!(pair.in_memory("SELECT SUM(n), AVG(n) FROM t"), "the rows were added up");
    assert_eq!(pair.agree("SELECT COUNT(m) FROM t"), Value::BigInt(4545));
    assert_eq!(pair.agree("SELECT SUM(n) FROM t"), Value::HugeInt(14995));
}

#[test]
fn a_table_in_memory_reads_its_rows_for_what_a_zone_map_does_not_hold() {
    let pair =
        Pair::new("memrows", "SELECT i % 7 AS n, 'v' || (i % 13) AS s FROM range(5000) r(i)");
    // A distinct count needs every value once and a zone map holds two of them, so this is the case
    // the file answers and memory does not. Both still say seven.
    assert!(!pair.in_memory("SELECT COUNT(DISTINCT n) FROM t"), "a zone map counted the values");
    assert_eq!(pair.agree("SELECT COUNT(DISTINCT n) FROM t"), Value::BigInt(7));
    // A filter puts a node between the aggregate and the table, and what is above a filter is a
    // question about some of the rows rather than about all of them.
    assert!(!pair.in_memory("SELECT COUNT(*) FROM t WHERE n > 1"), "a filter was ignored");
    assert_eq!(pair.agree("SELECT COUNT(*) FROM t WHERE n > 1"), Value::BigInt(3570));
    // A string column's ends are exact in a zone map, so this one memory does answer, which is worth
    // asserting beside the two above so that the line between them is drawn by a test.
    assert!(pair.in_memory("SELECT MIN(s), MAX(s) FROM t"), "the strings were walked");
    assert_eq!(pair.agree("SELECT MAX(s) FROM t"), Value::Varchar("v9".to_owned()));
}
