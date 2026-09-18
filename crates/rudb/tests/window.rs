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

fn doubles(values: &[f64]) -> Vec<Value> {
    values.iter().map(|&held| Value::Double(held)).collect()
}

#[test]
fn the_three_rankings_that_count_differ_only_on_the_rows_that_tie() {
    // Which is the whole reason there are three of them. Row number separates the tied rows, rank
    // gives both of them the first one's position and then leaves a gap, and dense rank numbers the
    // groups so it never leaves one.
    assert_eq!(answered("row_number() OVER (ORDER BY i)"), counts(&[1, 2, 3, 4, 5, 6]));
    assert_eq!(answered("rank() OVER (ORDER BY i)"), counts(&[1, 2, 2, 4, 5, 6]));
    assert_eq!(answered("dense_rank() OVER (ORDER BY i)"), counts(&[1, 2, 2, 3, 4, 5]));
    // Upstream's other spelling of the same function, which it reports as an alias.
    assert_eq!(answered("rank_dense() OVER (ORDER BY i)"), counts(&[1, 2, 2, 3, 4, 5]));
}

#[test]
fn the_two_rankings_that_divide_run_from_zero_to_one_and_from_above_zero_to_one() {
    assert_eq!(
        answered("percent_rank() OVER (ORDER BY i)"),
        doubles(&[0.0, 0.2, 0.2, 0.6, 0.8, 1.0])
    );
    assert_eq!(
        answered("cume_dist() OVER (ORDER BY i)"),
        doubles(&[1.0 / 6.0, 0.5, 0.5, 4.0 / 6.0, 5.0 / 6.0, 1.0])
    );
}

#[test]
fn a_ranking_with_no_order_sees_one_peer_group_and_answers_accordingly() {
    // Every row is a peer of every other, so rank is one everywhere and cume_dist is one everywhere,
    // while row_number still separates them because separating them is all it does.
    assert_eq!(answered("row_number() OVER ()"), counts(&[1, 2, 3, 4, 5, 6]));
    assert_eq!(answered("rank() OVER ()"), counts(&[1, 1, 1, 1, 1, 1]));
    assert_eq!(answered("percent_rank() OVER ()"), doubles(&[0.0; 6]));
    assert_eq!(answered("cume_dist() OVER ()"), doubles(&[1.0; 6]));
}

#[test]
fn a_ranking_ignores_the_frame_that_was_written_around_it() {
    // A rank is about the partition and a frame is about a row's neighbourhood, so naming one does
    // not change the other. These two columns are the same as the two without the frame above.
    let frame = "ORDER BY i ROWS BETWEEN 1 PRECEDING AND CURRENT ROW";
    assert_eq!(answered(&format!("rank() OVER ({frame})")), counts(&[1, 2, 2, 4, 5, 6]));
    assert_eq!(answered(&format!("row_number() OVER ({frame})")), counts(&[1, 2, 3, 4, 5, 6]));
}

#[test]
fn ntile_cuts_the_partition_into_buckets_and_the_bigger_ones_come_first() {
    // Six rows in four buckets are two, two, one and one. The remainder goes to the front, which is
    // the arrangement the standard asks for and the one the pin produces.
    assert_eq!(answered("ntile(2) OVER (ORDER BY i)"), counts(&[1, 1, 1, 2, 2, 2]));
    assert_eq!(answered("ntile(4) OVER (ORDER BY i)"), counts(&[1, 1, 2, 2, 3, 4]));
    // Each partition is cut on its own, so the three rows of the second one become two and one.
    let database = built();
    let sql = "SELECT ntile(2) OVER (PARTITION BY j ORDER BY i) FROM t ORDER BY j, i";
    assert_eq!(column(&database, sql, 0), counts(&[1, 1, 2, 1, 1, 2]));
}

#[test]
fn the_bucket_count_is_read_off_the_row_so_it_can_be_a_column() {
    // Upstream reads it per row rather than once for the partition, and a null there is a null
    // answer rather than a failure, which is the one place this family produces one.
    assert_eq!(
        answered("ntile(i) OVER (ORDER BY i)"),
        vec![
            Value::BigInt(1),
            Value::BigInt(1),
            Value::BigInt(1),
            Value::BigInt(2),
            Value::BigInt(3),
            Value::Null,
        ]
    );
}

#[test]
fn a_bucket_count_that_is_not_a_count_at_all_is_refused_in_upstreams_words() {
    let database = built();
    let connection = database.connect();
    let error = connection
        .query("SELECT ntile(0) OVER (ORDER BY i) FROM t")
        .expect_err("zero buckets is not a cut of anything");
    assert!(error.message().contains("Argument for ntile must be greater than zero"), "{error}");
}

#[test]
fn a_frame_distance_that_is_null_is_refused_and_one_that_is_negative_is_not() {
    // Two rules that look alike and are not. A null distance has no frame at all and upstream says
    // so, and a negative one has a frame that runs the other way, which usually covers nothing and
    // is an answer of null rather than an error.
    let database = built();
    let connection = database.connect();
    let error = connection
        .query("SELECT sum(i) OVER (ORDER BY i ROWS BETWEEN NULL PRECEDING AND CURRENT ROW) FROM t")
        .expect_err("a null distance is not a distance");
    assert!(error.message().contains("cannot be NULL"), "{error}");
    assert_eq!(
        answered("sum(i) OVER (ORDER BY i ROWS BETWEEN -1 PRECEDING AND CURRENT ROW)"),
        vec![Value::Null; 6]
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

/// A column of INTEGER values with nulls in it, which is what the five value windows answer with.
fn ints(values: &[Option<i32>]) -> Vec<Value> {
    values.iter().map(|held| held.map_or(Value::Null, Value::Integer)).collect()
}

/// A database holding `n(k INTEGER, v INTEGER)` where every other value is null.
///
/// The gaps are the point. `IGNORE NULLS` is the clause that separates counting rows from counting
/// values, and a column with no nulls in it cannot tell the two apart.
fn gapped() -> Database {
    let database = Database::new();
    let connection = database.connect();
    connection.execute("CREATE TABLE n(k INTEGER, v INTEGER)").expect("creates the table");
    connection
        .execute("INSERT INTO n VALUES (1,10),(2,NULL),(3,NULL),(4,40),(5,NULL),(6,60)")
        .expect("inserts six rows");
    database
}

/// The window column of a query over the gapped table, ordered so the rows arrive known.
fn gapped_answer(sql: &str) -> Vec<Value> {
    let database = gapped();
    let sql = format!("SELECT {sql} FROM n ORDER BY k");
    column(&database, &sql, 0)
}

#[test]
fn lag_and_lead_read_the_partition_and_the_frame_written_around_them_changes_nothing() {
    // The rule that is easy to get wrong, because every other window on this page reads the frame.
    // These two are about where a row sits in its partition, so a frame of one row and an exclusion
    // that drops the current row both leave the answer where it was.
    let plain = ints(&[None, Some(1), Some(2), Some(2), Some(3), Some(4)]);
    assert_eq!(answered("lag(i) OVER (ORDER BY i)"), plain);
    assert_eq!(
        answered("lag(i) OVER (ORDER BY i ROWS BETWEEN CURRENT ROW AND CURRENT ROW)"),
        plain
    );
    assert_eq!(
        answered(
            "lag(i) OVER (ORDER BY i ROWS BETWEEN UNBOUNDED PRECEDING AND UNBOUNDED FOLLOWING \
             EXCLUDE CURRENT ROW)"
        ),
        plain
    );
    assert_eq!(
        answered("lead(i) OVER (ORDER BY i)"),
        ints(&[Some(2), Some(2), Some(3), Some(4), None, None])
    );
}

#[test]
fn a_lag_takes_a_count_and_a_default_and_a_negative_count_is_a_lead() {
    // The count defaults to one, a count of zero is the row itself, and a negative count turns each
    // of these into the other rather than being refused. The default is answered only where the
    // count ran off the end of the partition.
    assert_eq!(
        answered("lag(i, 2) OVER (ORDER BY i)"),
        ints(&[None, None, Some(1), Some(2), Some(2), Some(3)])
    );
    assert_eq!(
        answered("lag(i, 1, -1) OVER (ORDER BY i)"),
        ints(&[Some(-1), Some(1), Some(2), Some(2), Some(3), Some(4)])
    );
    assert_eq!(
        answered("lag(i, 0) OVER (ORDER BY i)"),
        ints(&[Some(1), Some(2), Some(2), Some(3), Some(4), None])
    );
    assert_eq!(answered("lag(i, -1) OVER (ORDER BY i)"), answered("lead(i) OVER (ORDER BY i)"));
}

#[test]
fn a_count_that_is_null_answers_null_and_a_count_off_a_column_is_read_per_row() {
    // Both of these are about when the count is read. It is read off the current row and not once
    // for the partition, which is what lets a column supply it, and a null there is an answer of
    // null rather than the default.
    assert_eq!(gapped_answer("lag(k, NULL) OVER (ORDER BY k)"), vec![Value::Null; 6]);
    assert_eq!(gapped_answer("lag(k, k) OVER (ORDER BY k)"), vec![Value::Null; 6]);
}

#[test]
fn ignore_nulls_makes_lag_and_lead_count_values_rather_than_rows() {
    // The walk steps over every null it meets without spending a count on it, so the fourth row of
    // a column reading 10, null, null, 40 looks back past two nulls and lands on the 10.
    assert_eq!(
        gapped_answer("lag(v IGNORE NULLS) OVER (ORDER BY k)"),
        ints(&[None, Some(10), Some(10), Some(10), Some(40), Some(40)])
    );
    assert_eq!(
        gapped_answer("lead(v IGNORE NULLS) OVER (ORDER BY k)"),
        ints(&[Some(40), Some(40), Some(40), Some(60), Some(60), None])
    );
    assert_eq!(
        gapped_answer("lag(v, 2 IGNORE NULLS) OVER (ORDER BY k)"),
        ints(&[None, None, None, None, Some(10), Some(10)])
    );
}

#[test]
fn first_value_and_last_value_read_the_frame_and_obey_its_exclusion() {
    // Which is exactly what lag and lead do not do, and it is the same six rows either way, so the
    // two tests read as a pair. The default frame ends at the current row's peer group, which is
    // why the tied rows both answer 2 rather than one of them answering it.
    assert_eq!(
        answered("last_value(i) OVER (ORDER BY i)"),
        ints(&[Some(1), Some(2), Some(2), Some(3), Some(4), None])
    );
    assert_eq!(
        answered(
            "first_value(i) OVER (ORDER BY i ROWS BETWEEN UNBOUNDED PRECEDING AND UNBOUNDED \
             FOLLOWING EXCLUDE CURRENT ROW)"
        ),
        ints(&[Some(2), Some(1), Some(1), Some(1), Some(1), Some(1)])
    );
    assert_eq!(
        answered("first_value(i) OVER (ORDER BY i ROWS BETWEEN 3 PRECEDING AND 2 PRECEDING)"),
        ints(&[None, None, Some(1), Some(1), Some(2), Some(2)])
    );
}

#[test]
fn nth_value_counts_from_one_and_answers_null_wherever_the_count_does_not_reach() {
    // Four ways to get a null out of it and only one of them is the frame running out. Zero, a
    // negative count and a null count are the other three, and none of them is an error.
    let whole = "ROWS BETWEEN UNBOUNDED PRECEDING AND UNBOUNDED FOLLOWING";
    assert_eq!(
        answered(&format!("nth_value(i, 3) OVER (ORDER BY i {whole})")),
        ints(&[Some(2); 6])
    );
    assert_eq!(
        answered(&format!("nth_value(i, 10) OVER (ORDER BY i {whole})")),
        vec![Value::Null; 6]
    );
    assert_eq!(answered("nth_value(i, 0) OVER (ORDER BY i)"), vec![Value::Null; 6]);
    assert_eq!(answered("nth_value(i, -1) OVER (ORDER BY i)"), vec![Value::Null; 6]);
    assert_eq!(answered("nth_value(i, NULL) OVER (ORDER BY i)"), vec![Value::Null; 6]);
    // Read off the column, so each row counts to a different place in the same frame.
    assert_eq!(
        gapped_answer(&format!("nth_value(k, k) OVER (ORDER BY k {whole})")),
        ints(&[Some(1), Some(2), Some(3), Some(4), Some(5), Some(6)])
    );
}

#[test]
fn ignore_nulls_makes_the_picking_windows_count_values_too() {
    // Same clause and the same meaning as it has on lag, which is worth pinning on both because the
    // two answer through different code and the clause is written the same way in the query.
    assert_eq!(
        gapped_answer("first_value(v IGNORE NULLS) OVER (ORDER BY k)"),
        ints(&[Some(10); 6])
    );
    assert_eq!(
        gapped_answer(
            "last_value(v IGNORE NULLS) OVER (ORDER BY k ROWS BETWEEN UNBOUNDED PRECEDING AND \
             CURRENT ROW)"
        ),
        ints(&[Some(10), Some(10), Some(10), Some(40), Some(40), Some(60)])
    );
    assert_eq!(
        gapped_answer(
            "nth_value(v, 2 IGNORE NULLS) OVER (ORDER BY k ROWS BETWEEN UNBOUNDED PRECEDING AND \
             UNBOUNDED FOLLOWING)"
        ),
        ints(&[Some(40); 6])
    );
}

#[test]
fn a_value_window_answers_the_type_of_the_column_it_read() {
    // Not an ANY and not a widened one. The pin says VARCHAR for a lag over a string column and
    // INTEGER for the other four over an integer one, which is what makes the default cast to the
    // column's type rather than the other way round.
    let database = built();
    let sql = "SELECT typeof(lag(j) OVER (ORDER BY i)), typeof(first_value(i) OVER ()), \
               typeof(nth_value(i, 1) OVER ()) FROM t LIMIT 1";
    let connection = database.connect();
    let result = connection.query(sql).expect("the query runs");
    let row: Vec<Value> = result.rows().next().expect("one row").to_vec();
    assert_eq!(
        row,
        vec![
            Value::Varchar("VARCHAR".to_owned()),
            Value::Varchar("INTEGER".to_owned()),
            Value::Varchar("INTEGER".to_owned()),
        ]
    );
}

#[test]
fn a_default_of_another_type_is_cast_to_the_columns_and_one_that_will_not_cast_says_so() {
    // Upstream casts the default rather than widening the answer, so 0.5 into an INTEGER column is
    // the integer 1 and the string z into one is a conversion error at run time. Both of those are
    // the cast talking and neither is a binder error.
    assert_eq!(
        answered("lag(i, 1, 0.5) OVER (ORDER BY i)"),
        ints(&[Some(1), Some(1), Some(2), Some(2), Some(3), Some(4)])
    );
    let database = built();
    let connection = database.connect();
    let error = connection
        .query("SELECT lag(i, 1, 'z') OVER (ORDER BY i) FROM t")
        .expect_err("z is not an integer");
    assert!(error.message().contains("Could not convert string 'z'"), "{error}");
}

#[test]
fn distinct_inside_a_value_window_is_refused_the_way_the_pin_refuses_it() {
    // Doubled quotes and all. There is nothing for a DISTINCT to collapse when the call picks one
    // row rather than folding several, and upstream says so rather than ignoring it.
    let database = built();
    let connection = database.connect();
    let error = connection
        .query("SELECT first_value(DISTINCT i) OVER (ORDER BY i) FROM t")
        .expect_err("a distinct value window is refused");
    assert!(
        error
            .message()
            .contains("DISTINCT is not implemented for the window function \"\"first_value\"\""),
        "{error}"
    );
}
