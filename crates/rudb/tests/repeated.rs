//! An aggregate call that appears twice in one aggregate is folded once and read twice.
//!
//! The operator folds one accumulator for a call and lets the later copies finish off it, so an
//! aggregate asking for the same count three times keeps one counter and reads it three times.
//!
//! A query cannot ask for the same call twice by writing it twice, because the binder gives both
//! copies one slot in the aggregate and two references to it from the projection above. What makes
//! duplicates is the rewrites. Reading an average off a sum of the same column leaves a count of that
//! column behind for the division, and that count is appended without looking at what the query
//! already asked for, so `SUM(x), AVG(x), COUNT(x)` reaches the operator as `sum(x), count(x),
//! count(x)`. TPC-H q01 gets there a second way as well: both of the columns it averages are `NOT
//! NULL`, so the null free pass turns both of the counts the averages left behind into `count(*)`,
//! which is a third copy of the row count the query already asked for.
//!
//! Nothing above the operator is supposed to notice any of this. The output still has one column per
//! call in the order the plan asked for them, so what is checked here is that the repeated column
//! holds what the call would have answered on its own.
//!
//! # What these are guarding
//!
//! The copy has to read the state of the call it repeats and not its own, because its own is built
//! like every other one and then never folded into. A copy that read its own state answers zero for a
//! count, and since the appended count is the divisor of an average, the average reading it comes back
//! as a division by zero rather than as a visibly empty column. Every case below compares against the
//! same query written longhand for that reason.
//!
//! Two calls are only the same call when they would have been fed the same rows. A different
//! argument, a `FILTER` on one of them and a `DISTINCT` on one of them all make two calls that look
//! alike and count different rows, so each of those is here with an answer that differs from the
//! plain one.
//!
//! The shapes that produce a duplicate are checked to still produce one. They come out of the
//! optimizer rather than out of the query, so a pass that learned to reuse a count it already has
//! would leave every test here passing over a plan with nothing left to share, and the next person to
//! touch the sharing would have no coverage and no way to find that out.

use rudb::Database;
use rudb_common::Value;

/// Every row of a result, as values, so an ordered answer can be compared whole.
fn rows(database: &Database, query: &str) -> Vec<Vec<Value>> {
    database.query(query).expect("the query ran").rows().collect()
}

/// Asserts that `query` answers what `wanted` answers, and that it answered something.
///
/// The two are run on one database rather than two, because what is being checked is a decision the
/// aggregate makes about its own call list and not anything about the data.
fn same(database: &Database, query: &str, wanted: &str) {
    let got = rows(database, query);
    assert!(!got.is_empty(), "{query} answered nothing, so it proved nothing");
    assert_eq!(got, rows(database, wanted), "{query} disagrees with {wanted}");
}

/// The list of aggregate calls the planner leaves in `query`, as `EXPLAIN` prints it.
fn calls(database: &Database, query: &str) -> String {
    let plan = database.query(&format!("EXPLAIN {query}")).expect("the query planned");
    let Value::Varchar(text) = plan.value_at(0, 1) else { panic!("the plan came back wrong") };
    let line = text
        .lines()
        .find(|line| line.contains("aggregates=["))
        .unwrap_or_else(|| panic!("{query} has no aggregate in its plan:\n{text}"))
        .to_string();
    let from = line.find("aggregates=[").expect("the line holds the list");
    let to = line[from..].find(']').expect("the list is closed") + from;
    line[from..=to].to_string()
}

/// Asserts that `query` reaches the operator asking for `call` more than once.
fn twice(database: &Database, query: &str, call: &str) {
    let list = calls(database, query);
    let found = list.matches(call).count();
    assert!(found > 1, "{query} asks for {call} {found} times, so it shares nothing: {list}");
}

/// A table of four groups over 97 rows, with a column of each shape a shared count can be over.
///
/// `whole` is an integer with no nulls and `money` a decimal with none, which is the pair q01 sums and
/// averages. `gappy` has nulls, so a count of it is smaller than the row count and a group of nothing
/// but nulls sums to null rather than to zero. `empty` is null in every row, so every group of it
/// counts zero, which is the divisor a copy reading its own state would produce everywhere.
fn built() -> Database {
    let database = Database::new();
    let connection = database.connect();
    connection.execute("SET threads = 1").expect("sets the thread count");
    connection
        .execute(
            "CREATE TABLE t AS
             SELECT i % 4 AS k,
                    CAST(i * 3 AS INTEGER) AS whole,
                    CAST(i AS DECIMAL(15, 2)) AS money,
                    CASE WHEN i % 5 = 0 THEN NULL ELSE CAST(i AS BIGINT) END AS gappy,
                    CAST(NULL AS BIGINT) AS empty
             FROM range(0, 97) AS r(i)",
        )
        .expect("builds the table");
    database
}

/// Every column, one at a time so that a wrong answer names the column it came from. The count the
/// query asks for and the count the average needs are the same call, and the average divides by the
/// one the query asked for.
#[test]
fn a_count_the_query_asked_for_is_the_count_an_average_divides_by() {
    let database = built();
    for column in ["whole", "money", "gappy", "empty"] {
        let query = format!(
            "SELECT k, SUM({column}), AVG({column}), COUNT({column}) FROM t GROUP BY k ORDER BY k"
        );
        twice(&database, &query, "count(");
        same(
            &database,
            &query,
            &format!(
                "SELECT k, SUM({column}), SUM({column}) / COUNT({column}), COUNT({column})
                 FROM t GROUP BY k ORDER BY k"
            ),
        );
    }
}

/// The same without a `GROUP BY`, which is a different path through the aggregate: one group made up
/// front and every call folded by vector.
#[test]
fn the_same_over_the_whole_table_with_no_grouping() {
    let database = built();
    for column in ["whole", "money", "gappy", "empty"] {
        let query = format!("SELECT SUM({column}), AVG({column}), COUNT({column}) FROM t");
        twice(&database, &query, "count(");
        same(
            &database,
            &query,
            &format!(
                "SELECT SUM({column}), SUM({column}) / COUNT({column}), COUNT({column}) FROM t"
            ),
        );
    }
}

/// Two columns at once with the pairs interleaved, which is q01's shape and the one where a copy wired
/// one position off gives a number that is some other column's count.
#[test]
fn two_columns_interleaved_each_divide_by_their_own_count() {
    let database = built();
    let query = "SELECT k, COUNT(gappy), SUM(whole), SUM(gappy), AVG(whole), AVG(gappy),
                        COUNT(whole), COUNT(*)
                 FROM t GROUP BY k ORDER BY k";
    twice(&database, query, "count(#0.1::INTEGER)");
    twice(&database, query, "count(#0.2::BIGINT)");
    same(
        &database,
        query,
        "SELECT k, COUNT(gappy), SUM(whole), SUM(gappy), SUM(whole) / COUNT(whole),
                SUM(gappy) / COUNT(gappy), COUNT(whole), COUNT(*)
         FROM t GROUP BY k ORDER BY k",
    );
}

/// A group of nothing but nulls, where the shared count is zero and the average of it is null. A copy
/// that read its own state would answer zero here too, so the group that shows the difference is one
/// whose rows do count.
#[test]
fn a_group_of_nothing_but_nulls_averages_to_null_and_the_rest_do_not() {
    let database = Database::new();
    let connection = database.connect();
    connection
        .execute(
            "CREATE TABLE u AS SELECT i % 3 AS k,
                                      CASE WHEN i % 3 = 1 THEN NULL ELSE CAST(i AS BIGINT) END AS v
                               FROM range(0, 30) AS r(i)",
        )
        .expect("builds the table");
    let query = "SELECT k, SUM(v), AVG(v), COUNT(v) FROM u GROUP BY k ORDER BY k";
    twice(&database, query, "count(");
    same(
        &database,
        query,
        "SELECT k, SUM(v), SUM(v) / COUNT(v), COUNT(v) FROM u GROUP BY k ORDER BY k",
    );
    let answered = rows(&database, query);
    assert_eq!(answered[1][2], Value::Null, "a group of nothing but nulls did not average to null");
    assert_ne!(answered[0][2], Value::Null, "every group is null, so this proves nothing");
    assert_eq!(answered[1][3], Value::BigInt(0), "a group of nothing but nulls counted something");
}

/// Two calls that look alike and count different rows. A `FILTER` on one of them, a `DISTINCT` on one
/// of them and a different argument each have to keep both folding, and each pair here is one where
/// the two answers differ, so a copy that shared a state would answer the other one's number.
#[test]
fn calls_that_are_fed_different_rows_are_not_the_same_call() {
    let database = built();
    same(
        &database,
        "SELECT k, SUM(whole), AVG(whole), COUNT(whole) FILTER (WHERE whole > 100)
         FROM t GROUP BY k ORDER BY k",
        "SELECT k, SUM(whole), SUM(whole) / COUNT(whole),
                COUNT(CASE WHEN whole > 100 THEN whole END)
         FROM t GROUP BY k ORDER BY k",
    );
    same(
        &database,
        "SELECT k, SUM(gappy), AVG(gappy), COUNT(DISTINCT gappy) FROM t GROUP BY k ORDER BY k",
        "SELECT k, SUM(gappy), SUM(gappy) / COUNT(gappy), COUNT(DISTINCT gappy)
         FROM t GROUP BY k ORDER BY k",
    );
    same(
        &database,
        "SELECT k, SUM(whole), AVG(whole), COUNT(gappy) FROM t GROUP BY k ORDER BY k",
        "SELECT k, SUM(whole), SUM(whole) / COUNT(whole), COUNT(gappy)
         FROM t GROUP BY k ORDER BY k",
    );
    let filtered = rows(
        &database,
        "SELECT k, COUNT(whole), COUNT(whole) FILTER (WHERE whole > 100)
         FROM t GROUP BY k ORDER BY k",
    );
    for row in &filtered {
        assert_ne!(row[1], row[2], "the filter kept every row, so this proves nothing");
    }
}

/// Enough rows over enough groups on several threads that the radix partitioned tables are merged
/// before anything is finished, because a copy that read a state the merge never wrote to is a way for
/// this to be right on one thread and wrong on four.
#[test]
fn a_shared_count_over_a_parallel_group_by() {
    let database = Database::new();
    let connection = database.connect();
    connection.execute("SET threads = 4").expect("sets the thread count");
    connection
        .execute(
            "CREATE TABLE wide AS SELECT i % 5000 AS k, CAST(i AS BIGINT) AS v
             FROM range(0, 200000) AS r(i)",
        )
        .expect("builds the table");
    let query = "SELECT k, SUM(v), AVG(v), COUNT(v) FROM wide GROUP BY k";
    twice(&database, query, "count(");
    let got = rows(
        &database,
        &format!(
            "SELECT COUNT(*), SUM(total), SUM(mean), SUM(counted)
             FROM ({query}) AS g(k, total, mean, counted)"
        ),
    );
    let wanted = rows(
        &database,
        "SELECT COUNT(*), SUM(total), SUM(mean), SUM(counted)
         FROM (SELECT k, SUM(v) AS total, SUM(v) / COUNT(v) AS mean, COUNT(v) AS counted
               FROM wide GROUP BY k) AS g",
    );
    assert_eq!(got, wanted, "a parallel group by lost rows or a copy read an unfolded state");
    assert_eq!(got[0][3], Value::HugeInt(200_000), "the counts do not add up to the rows");
}
