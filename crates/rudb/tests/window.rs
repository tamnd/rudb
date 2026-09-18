//! What a window answers, checked against what DuckDB answers.
//!
//! Every expected column in this file was read off the pinned binary rather than worked out from
//! the standard. Windows are the part of SQL where a careful reading of the standard and a careful
//! reading of the implementation disagree often enough that only one of them is worth testing
//! against, and the one worth testing against is the one the corpus was recorded from.
//!
//! The numbers are small on purpose. A window is a sort plus a pass over each partition and both of
//! those are already tested at scale elsewhere. What is easy to get wrong here is which rows a
//! frame covers, and six rows with a tie in them and a null in them says that better than six
//! million would.

use rudb::Database;
use rudb_common::Value;

/// A database holding `t(i INTEGER, j VARCHAR)` with a tie, a null and two partitions in it.
///
/// The tie is what separates `RANGE` from `ROWS`, the null is what the frame has to carry through
/// without treating it as a value, and the two partitions are what a single pass has to keep apart.
fn built() -> Database {
    let database = Database::new();
    let connection = database.connect();
    connection.execute("CREATE TABLE t(i INTEGER, j VARCHAR)").expect("creates the table");
    connection
        .execute("INSERT INTO t VALUES (1,'a'),(2,'a'),(2,'a'),(3,'b'),(4,'b'),(NULL,'b')")
        .expect("inserts six rows");
    database
}

/// One column of a query's answer, which is the only thing any of these look at.
fn column(database: &Database, sql: &str, at: usize) -> Vec<Value> {
    let connection = database.connect();
    let result = connection.query(sql).unwrap_or_else(|error| panic!("{sql} should run: {error}"));
    result.rows().map(|row| row[at].clone()).collect()
}

/// The window column of a query that orders by `i` so the rows arrive in a known order.
fn answered(sql: &str) -> Vec<Value> {
    let database = built();
    let sql = format!("SELECT {sql} FROM t ORDER BY i, j");
    column(&database, &sql, 0)
}

fn totals(values: &[i128]) -> Vec<Value> {
    values.iter().map(|&held| Value::HugeInt(held)).collect()
}

fn counts(values: &[i64]) -> Vec<Value> {
    values.iter().map(|&held| Value::BigInt(held)).collect()
}

#[test]
fn a_window_with_no_over_at_all_totals_the_whole_input() {
    // Not a running total. With no `ORDER BY` every row of the partition is a peer of every other,
    // so the default frame covers all of them and every row gets the same answer.
    assert_eq!(answered("sum(i) OVER ()"), totals(&[12, 12, 12, 12, 12, 12]));
}

#[test]
fn a_partition_divides_the_input_and_nothing_crosses_the_line() {
    assert_eq!(answered("sum(i) OVER (PARTITION BY j)"), totals(&[5, 5, 5, 7, 7, 7]));
}

#[test]
fn an_order_with_no_frame_is_a_running_total_over_peer_groups() {
    // The two rows that tie on 2 both get 5 and not 3 and 5. That is the whole difference between
    // the default `RANGE` frame and the `ROWS` frame below, and it is the single easiest thing to
    // get wrong in a window implementation.
    assert_eq!(answered("sum(i) OVER (ORDER BY i)"), totals(&[1, 5, 5, 8, 12, 12]));
    assert_eq!(
        answered("sum(i) OVER (ORDER BY i ROWS BETWEEN 1 PRECEDING AND CURRENT ROW)"),
        totals(&[1, 3, 4, 5, 7, 4])
    );
}

#[test]
fn a_null_key_is_a_partition_and_a_peer_group_like_any_other_value() {
    // The null sorts last under `ASC NULLS LAST` and is its own peer group, so the running total
    // reaches it after everything else and it carries the whole total.
    assert_eq!(answered("count(*) OVER (PARTITION BY j ORDER BY i)"), counts(&[1, 3, 3, 1, 2, 3]));
}

#[test]
fn an_unbounded_frame_in_both_directions_is_the_partition_whatever_the_order_says() {
    assert_eq!(
        answered(
            "count(*) OVER (ORDER BY i ROWS BETWEEN UNBOUNDED PRECEDING AND UNBOUNDED FOLLOWING)"
        ),
        counts(&[6, 6, 6, 6, 6, 6])
    );
}

#[test]
fn a_groups_distance_counts_peer_groups_and_not_rows() {
    // One group back from the pair of twos is the single one, so both twos see 1 and 2 and 2 and
    // answer 5. One group back from the three is that same pair, so it sees 2 and 2 and 3.
    assert_eq!(
        answered("sum(i) OVER (ORDER BY i GROUPS BETWEEN 1 PRECEDING AND CURRENT ROW)"),
        totals(&[1, 5, 5, 7, 7, 4])
    );
}

#[test]
fn every_exclusion_takes_a_different_piece_out_of_the_same_frame() {
    // The three of them differ only on the tied rows, which is the point. Current row drops the row
    // itself, group drops its whole peer group, and ties drops the group but keeps the row, so on
    // a row with no peers the first and the third agree and the second does not.
    let frame = "ORDER BY i RANGE BETWEEN UNBOUNDED PRECEDING AND CURRENT ROW";
    assert_eq!(
        answered(&format!("sum(i) OVER ({frame} EXCLUDE CURRENT ROW)")),
        vec![
            Value::Null,
            Value::HugeInt(3),
            Value::HugeInt(3),
            Value::HugeInt(5),
            Value::HugeInt(8),
            Value::HugeInt(12),
        ]
    );
    assert_eq!(
        answered(&format!("sum(i) OVER ({frame} EXCLUDE GROUP)")),
        vec![
            Value::Null,
            Value::HugeInt(1),
            Value::HugeInt(1),
            Value::HugeInt(5),
            Value::HugeInt(8),
            Value::HugeInt(12),
        ]
    );
    assert_eq!(
        answered(&format!("sum(i) OVER ({frame} EXCLUDE TIES)")),
        totals(&[1, 3, 3, 8, 12, 12])
    );
}

#[test]
fn a_frame_that_covers_nothing_is_null_and_not_zero() {
    // `3 PRECEDING AND 2 PRECEDING` has nothing in it until there are two rows behind the current
    // one. A sum over no rows is null, the same as a sum over an empty table, and a count over no
    // rows is zero, because that is what the two functions do with nothing rather than anything to
    // do with frames.
    assert_eq!(
        answered("sum(i) OVER (ORDER BY i ROWS BETWEEN 3 PRECEDING AND 2 PRECEDING)"),
        vec![
            Value::Null,
            Value::Null,
            Value::HugeInt(1),
            Value::HugeInt(3),
            Value::HugeInt(4),
            Value::HugeInt(5),
        ]
    );
    assert_eq!(
        answered("count(*) OVER (ORDER BY i ROWS BETWEEN 3 PRECEDING AND 2 PRECEDING)"),
        counts(&[0, 0, 1, 2, 2, 2])
    );
}

#[test]
fn a_frame_that_runs_forward_from_the_current_row_reaches_the_end_of_the_partition() {
    assert_eq!(
        answered("min(i) OVER (ORDER BY i ROWS BETWEEN CURRENT ROW AND UNBOUNDED FOLLOWING)"),
        vec![
            Value::Integer(1),
            Value::Integer(2),
            Value::Integer(2),
            Value::Integer(3),
            Value::Integer(4),
            Value::Null,
        ]
    );
}

#[test]
fn a_distance_can_be_a_column_so_every_row_gets_a_frame_of_its_own_size() {
    let database = Database::new();
    let connection = database.connect();
    connection.execute("CREATE TABLE u(i INTEGER, j INTEGER)").expect("creates the table");
    connection
        .execute("INSERT INTO u VALUES (1,1),(2,1),(3,2),(4,2),(5,2)")
        .expect("inserts five rows");
    let sql = "SELECT sum(i) OVER (ORDER BY i ROWS BETWEEN j PRECEDING AND CURRENT ROW) \
               FROM u ORDER BY i";
    assert_eq!(column(&database, sql, 0), totals(&[1, 3, 6, 9, 12]));
}

#[test]
fn a_distinct_window_counts_each_value_once_within_its_partition() {
    let database = built();
    let sql = "SELECT count(DISTINCT i) OVER (PARTITION BY j) FROM t ORDER BY j, i";
    assert_eq!(column(&database, sql, 0), counts(&[2, 2, 2, 2, 2, 2]));
}

#[test]
fn two_windows_that_disagree_are_two_operators_and_both_answers_are_right() {
    let database = Database::new();
    let connection = database.connect();
    connection.execute("CREATE TABLE u(i INTEGER, j INTEGER)").expect("creates the table");
    connection
        .execute("INSERT INTO u VALUES (1,1),(2,1),(3,2),(4,2),(5,2)")
        .expect("inserts five rows");
    let sql = "SELECT sum(i) OVER (PARTITION BY j ORDER BY i), count(*) OVER (ORDER BY i DESC) \
               FROM u ORDER BY i";
    assert_eq!(column(&database, sql, 0), totals(&[1, 3, 3, 7, 12]));
    assert_eq!(column(&database, sql, 1), counts(&[5, 4, 3, 2, 1]));
}

#[test]
fn a_window_over_a_grouped_block_reads_the_groups_and_not_the_rows() {
    let database = built();
    let sql = "SELECT sum(count(i)) OVER () FROM t GROUP BY j ORDER BY j";
    assert_eq!(column(&database, sql, 0), totals(&[5, 5]));
}

#[test]
fn a_window_over_no_rows_produces_no_rows_rather_than_one() {
    // The difference between a window and an ungrouped aggregate, which is worth a test because
    // they look alike everywhere else. `sum(i) FROM t WHERE false` answers one row holding null and
    // `sum(i) OVER () FROM t WHERE false` answers nothing at all.
    let database = built();
    assert!(column(&database, "SELECT sum(i) OVER () FROM t WHERE i > 100", 0).is_empty());
}

#[test]
fn a_window_in_an_order_by_decides_the_order_without_appearing_in_the_answer() {
    let database = Database::new();
    let connection = database.connect();
    connection.execute("CREATE TABLE u(i INTEGER, j INTEGER)").expect("creates the table");
    connection
        .execute("INSERT INTO u VALUES (1,1),(2,1),(3,2),(4,2),(5,2)")
        .expect("inserts five rows");
    let sql = "SELECT i FROM u ORDER BY sum(i) OVER (PARTITION BY j) DESC, i";
    let answer = column(&database, sql, 0);
    assert_eq!(
        answer,
        vec![
            Value::Integer(3),
            Value::Integer(4),
            Value::Integer(5),
            Value::Integer(1),
            Value::Integer(2),
        ]
    );
}

#[test]
fn a_range_frame_with_a_distance_says_it_is_not_done_rather_than_guessing() {
    // The one gap left, and it is a distance measured from the current row's order key rather than
    // from its position, so answering it is arithmetic over whatever type that key has. Saying so
    // is better than answering it as though it were a `ROWS` frame, which is what it is not.
    let database = built();
    let connection = database.connect();
    let error = connection
        .query("SELECT sum(i) OVER (ORDER BY i RANGE BETWEEN 1 PRECEDING AND CURRENT ROW) FROM t")
        .expect_err("that frame is not answered yet");
    assert!(error.message().contains("RANGE frame with an offset"), "{error}");
}

#[test]
fn a_window_answers_the_same_on_one_thread_as_on_eight() {
    // The rows reach the operator in whatever order the threads finished in, and the answer cannot
    // depend on that. The running total is the query that would notice, because every row of it has
    // a different answer and a row in the wrong place moves two of them.
    let mut answers = Vec::new();
    for threads in [1, 8] {
        let database = built();
        let connection = database.connect();
        connection.execute(&format!("SET threads = {threads}")).expect("sets the thread count");
        let sql = "SELECT sum(i) OVER (ORDER BY i ROWS BETWEEN 1 PRECEDING AND CURRENT ROW) \
                   FROM t ORDER BY i, j";
        answers.push(column(&database, sql, 0));
    }
    assert_eq!(answers[0], answers[1]);
    assert_eq!(answers[0], totals(&[1, 3, 4, 5, 7, 4]));
}
