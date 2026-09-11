//! End to end tests, from a string of SQL to rows.
//!
//! The layers below have their own tests and this file does not repeat them. `rudb-parse` proves
//! the grammar, `rudb-bind` proves that a query binds to the plan it should, and `rudb-exec` proves
//! that a plan produces the right rows. What is only testable here is the three of them agreeing:
//! a name a query writes has to be the name the binder resolves and the name the executor reads,
//! and a type the binder decided has to be the type the operator produces.

use rudb_common::{Field, LogicalType, Value, days_from_civil};

use crate::Database;

/// `t(x INTEGER, s VARCHAR)` with a null in it, plus an empty table to test the degenerate cases.
fn database() -> Database {
    let db = Database::new();
    db.create_table(
        "t",
        vec![Field::new("x", LogicalType::Integer), Field::new("s", LogicalType::Varchar)],
    )
    .unwrap();
    db.append(
        "t",
        &[
            vec![Value::Integer(3), Value::Varchar("a".to_string())],
            vec![Value::Integer(1), Value::Null],
            vec![Value::Integer(2), Value::Varchar("c".to_string())],
            vec![Value::Integer(1), Value::Varchar("a".to_string())],
        ],
    )
    .unwrap();
    db.create_table("empty", vec![Field::new("x", LogicalType::Integer)]).unwrap();
    db
}

/// Every row of a query, which is what most of these assert on.
fn rows(db: &Database, sql: &str) -> Vec<Vec<Value>> {
    db.query(sql).unwrap().rows().collect()
}

/// The message a query fails with.
fn failure(db: &Database, sql: &str) -> String {
    db.query(sql).unwrap_err().message().to_string()
}

fn integer(value: i32) -> Value {
    Value::Integer(value)
}

fn text(value: &str) -> Value {
    Value::Varchar(value.to_string())
}

#[test]
fn a_star_reads_every_column_in_order() {
    let db = database();
    assert_eq!(
        rows(&db, "SELECT * FROM t"),
        vec![
            vec![integer(3), text("a")],
            vec![integer(1), Value::Null],
            vec![integer(2), text("c")],
            vec![integer(1), text("a")],
        ]
    );
}

/// The query from the crate documentation, which is the smallest thing that touches all four
/// layers.
#[test]
fn a_filter_keeps_the_rows_the_predicate_is_true_for() {
    let db = database();
    assert_eq!(rows(&db, "SELECT x FROM t WHERE x > 1"), vec![vec![integer(3)], vec![integer(2)]]);
}

/// A predicate that is null is not a predicate that is true, so the null row goes out. This is the
/// rule that separates SQL from every language whose `if` takes a boolean.
#[test]
fn a_null_predicate_drops_the_row() {
    let db = database();
    assert_eq!(rows(&db, "SELECT x FROM t WHERE s = 'c'"), vec![vec![integer(2)]]);
    assert_eq!(
        rows(&db, "SELECT x FROM t WHERE s <> 'c'"),
        vec![vec![integer(3)], vec![integer(1)]]
    );
}

#[test]
fn a_query_with_no_table_still_runs() {
    let db = database();
    assert_eq!(rows(&db, "SELECT 1 + 1"), vec![vec![integer(2)]]);
}

#[test]
fn an_expression_takes_the_name_it_was_aliased_to() {
    let db = database();
    let result = db.query("SELECT x + 10 AS bumped FROM t WHERE x = 2").unwrap();
    assert_eq!(result.names(), ["bumped"]);
    assert_eq!(result.types(), [LogicalType::Integer]);
    assert_eq!(result.value_at(0, 0), integer(12));
}

#[test]
fn an_aggregate_over_no_rows_is_zero_and_null() {
    let db = database();
    assert_eq!(rows(&db, "SELECT count(*) FROM empty"), vec![vec![Value::BigInt(0)]]);
    assert_eq!(rows(&db, "SELECT sum(x) FROM empty"), vec![vec![Value::Null]]);
}

/// `count(*)` counts rows and `count(s)` counts the rows where `s` is not null. One null in the
/// table is enough to tell them apart.
#[test]
fn count_star_and_count_of_a_column_disagree_about_nulls() {
    let db = database();
    assert_eq!(rows(&db, "SELECT count(*) FROM t"), vec![vec![Value::BigInt(4)]]);
    assert_eq!(rows(&db, "SELECT count(s) FROM t"), vec![vec![Value::BigInt(3)]]);
}

#[test]
fn a_group_by_produces_one_row_per_distinct_value() {
    let db = database();
    let mut answer = rows(&db, "SELECT x, count(*) FROM t GROUP BY x");
    answer.sort_by_key(|row| format!("{:?}", row[0]));
    assert_eq!(
        answer,
        vec![
            vec![integer(1), Value::BigInt(2)],
            vec![integer(2), Value::BigInt(1)],
            vec![integer(3), Value::BigInt(1)],
        ]
    );
}

#[test]
fn a_having_clause_filters_groups_rather_than_rows() {
    let db = database();
    assert_eq!(
        rows(&db, "SELECT x, count(*) FROM t GROUP BY x HAVING count(*) > 1"),
        vec![vec![integer(1), Value::BigInt(2)]]
    );
}

/// Direction and null placement are two decisions, and `DESC` alone puts nulls first in DuckDB.
#[test]
fn order_by_puts_nulls_where_the_clause_says() {
    let db = database();
    assert_eq!(
        rows(&db, "SELECT s FROM t ORDER BY s NULLS LAST"),
        vec![vec![text("a")], vec![text("a")], vec![text("c")], vec![Value::Null]]
    );
    assert_eq!(
        rows(&db, "SELECT s FROM t ORDER BY s NULLS FIRST"),
        vec![vec![Value::Null], vec![text("a")], vec![text("a")], vec![text("c")]]
    );
}

#[test]
fn order_by_and_limit_and_offset_compose() {
    let db = database();
    assert_eq!(
        rows(&db, "SELECT x FROM t ORDER BY x DESC LIMIT 2 OFFSET 1"),
        vec![vec![integer(2)], vec![integer(1)]]
    );
}

#[test]
fn distinct_collapses_equal_rows() {
    let db = database();
    let mut answer = rows(&db, "SELECT DISTINCT x FROM t");
    answer.sort_by_key(|row| format!("{:?}", row[0]));
    assert_eq!(answer, vec![vec![integer(1)], vec![integer(2)], vec![integer(3)]]);
}

#[test]
fn a_join_matches_on_its_condition() {
    let db = database();
    assert_eq!(
        rows(&db, "SELECT a.x, b.s FROM t AS a JOIN t AS b ON a.x = b.x WHERE a.x = 2"),
        vec![vec![integer(2), text("c")]]
    );
}

#[test]
fn a_left_join_keeps_the_left_row_and_pads_the_right() {
    let db = database();
    assert_eq!(
        rows(&db, "SELECT t.x, e.x FROM t LEFT JOIN empty AS e ON t.x = e.x WHERE t.x = 3"),
        vec![vec![integer(3), Value::Null]]
    );
}

#[test]
fn a_union_deduplicates_and_union_all_does_not() {
    let db = database();
    assert_eq!(rows(&db, "SELECT 1 UNION ALL SELECT 1"), vec![vec![integer(1)], vec![integer(1)]]);
    assert_eq!(rows(&db, "SELECT 1 UNION SELECT 1"), vec![vec![integer(1)]]);
}

#[test]
fn a_subquery_in_the_from_clause_is_a_table() {
    let db = database();
    assert_eq!(
        rows(&db, "SELECT inner_query.total FROM (SELECT count(*) AS total FROM t) AS inner_query"),
        vec![vec![Value::BigInt(4)]]
    );
}

#[test]
fn a_case_expression_picks_the_first_arm_that_holds() {
    let db = database();
    assert_eq!(
        rows(&db, "SELECT CASE WHEN x > 2 THEN 'big' ELSE 'small' END FROM t ORDER BY x"),
        vec![vec![text("small")], vec![text("small")], vec![text("small")], vec![text("big")]]
    );
}

/// A comparison against a string reads the string as the other side's type rather than printing
/// the other side. Every answer here is the answer DuckDB gives.
#[test]
fn a_comparison_with_a_string_happens_in_the_other_type() {
    let db = database();
    // Text comparison would say these two are different, since the day is not padded. The cast is
    // spelled out because DATE '2013-07-15' is a typed literal and the transformer does not cover
    // that rule yet.
    assert_eq!(
        rows(&db, "SELECT CAST('2013-07-15' AS DATE) = '2013-7-15'"),
        vec![vec![Value::Boolean(true)]]
    );
    assert_eq!(
        rows(&db, "SELECT CAST('2013-07-15' AS DATE) >= '2013-07-01'"),
        vec![vec![Value::Boolean(true)]]
    );
    // Text comparison would say ten is less than nine.
    assert_eq!(rows(&db, "SELECT 10 > '9'"), vec![vec![Value::Boolean(true)]]);
    assert_eq!(rows(&db, "SELECT TRUE = 'true'"), vec![vec![Value::Boolean(true)]]);
    // Not here yet: DuckDB answers true to 1 = '1.0', because its string to integer cast rounds
    // rather than refusing a decimal point, and rounds half away from zero. That is a difference
    // in the cast rather than in this rule, and it belongs with the cast.
}

/// 2013-07-15 at a time of day, which is the day ClickBench asks about.
fn moment(hours: i64, minutes: i64, seconds: i64) -> Value {
    let day = i64::from(days_from_civil(2013, 7, 15)) * 86_400_000_000;
    Value::Timestamp(day + hours * 3_600_000_000 + minutes * 60_000_000 + seconds * 1_000_000)
}

/// The two shapes ClickBench needs, which are query 19 and query 43 with the column renamed.
///
/// `EXTRACT` is not a function in the grammar and is a call to `date_part` by the time the binder
/// sees it, so this is also the test that the rewrite survives the trip. The truncation keeps the
/// type it was given, which is why the second query can sort by it and get times rather than text.
#[test]
fn a_timestamp_can_be_taken_apart_and_grouped_by() {
    let db = Database::new();
    db.create_table("hits", vec![Field::new("ts", LogicalType::Timestamp)]).unwrap();
    db.append("hits", &[vec![moment(10, 23, 45)], vec![moment(10, 23, 7)], vec![moment(11, 5, 0)]])
        .unwrap();
    assert_eq!(
        rows(&db, "SELECT extract(minute FROM ts) AS m, COUNT(*) FROM hits GROUP BY m ORDER BY m"),
        vec![vec![Value::BigInt(5), Value::BigInt(1)], vec![Value::BigInt(23), Value::BigInt(2)],]
    );
    assert_eq!(
        rows(
            &db,
            "SELECT DATE_TRUNC('hour', ts) AS h, COUNT(*) AS n FROM hits \
             GROUP BY DATE_TRUNC('hour', ts) ORDER BY DATE_TRUNC('hour', ts)"
        ),
        vec![vec![moment(10, 0, 0), Value::BigInt(2)], vec![moment(11, 0, 0), Value::BigInt(1)],]
    );
}

/// The part is a string wherever it came from, and a specifier that names nothing is DuckDB's
/// message rather than a panic in a match arm.
#[test]
fn a_date_part_reads_its_specifier_three_ways_and_refuses_a_fourth() {
    let db = database();
    let stamp = "CAST('2013-07-15 10:23:45' AS TIMESTAMP)";
    for sql in [
        format!("SELECT extract(minute FROM {stamp})"),
        format!("SELECT extract('minute' FROM {stamp})"),
        format!("SELECT date_part('minute', {stamp})"),
    ] {
        assert_eq!(rows(&db, &sql), vec![vec![Value::BigInt(23)]], "{sql}");
    }
    assert!(failure(&db, &format!("SELECT date_part('qtr', {stamp})")).contains("qtr"));
}

/// ClickBench query 29 with the aggregates cut down to one, which is the last of the forty three to
/// plan and the only one that needs a regular expression.
///
/// The pattern and the replacement are the ones the benchmark ships, character for character, which
/// is the point of the exercise. A referer with no path does not match, and the answer for a row
/// that does not match is the referer itself, which is what puts a whole URL in the group list
/// rather than dropping the row.
#[test]
fn a_referer_can_be_cut_down_to_its_host_and_grouped_by() {
    let db = Database::new();
    db.create_table("hits", vec![Field::new("Referer", LogicalType::Varchar)]).unwrap();
    db.append(
        "hits",
        &[
            vec![text("http://www.example.com/a/b")],
            vec![text("https://example.com/")],
            vec![text("http://other.org/x?y=1")],
            vec![text("")],
        ],
    )
    .unwrap();
    assert_eq!(
        rows(
            &db,
            "SELECT REGEXP_REPLACE(Referer, '^https?://(?:www\\.)?([^/]+)/.*$', '\\1') AS k, \
             COUNT(*) AS c FROM hits WHERE Referer <> '' GROUP BY k ORDER BY c DESC, k"
        ),
        vec![
            vec![text("example.com"), Value::BigInt(2)],
            vec![text("other.org"), Value::BigInt(1)]
        ]
    );
}

/// The other three, and the two ways a query can get a regular expression wrong. Every answer here
/// was read off DuckDB before it was written down.
#[test]
fn the_regular_expression_functions_answer_the_way_duckdb_does() {
    let db = database();
    assert_eq!(rows(&db, "SELECT regexp_matches('abc', 'b')"), vec![vec![Value::Boolean(true)]]);
    assert_eq!(
        rows(&db, "SELECT regexp_full_match('abc', 'a')"),
        vec![vec![Value::Boolean(false)]]
    );
    assert_eq!(
        rows(&db, "SELECT regexp_extract('abc123', '([a-z]+)([0-9]+)', 2)"),
        vec![vec![text("123")]]
    );
    assert_eq!(rows(&db, "SELECT regexp_extract('abc', 'z')"), vec![vec![text("")]]);
    assert_eq!(
        rows(&db, "SELECT regexp_replace('aXbXc', 'X', '-', 'g')"),
        vec![vec![text("a-b-c")]]
    );
    assert_eq!(rows(&db, "SELECT regexp_replace(NULL, 'a', 'b')"), vec![vec![Value::Null]]);
    // The column is the thing that varies and the pattern is not, which is the shape the kernel
    // compiles once per vector rather than once per row.
    assert_eq!(
        rows(&db, "SELECT s FROM t WHERE regexp_matches(s, '^[ac]$')"),
        vec![vec![text("a")], vec![text("c")], vec![text("a")]]
    );
    assert!(failure(&db, "SELECT regexp_matches('a', '(')").contains("missing )"));
    assert!(
        failure(&db, "SELECT regexp_replace('a', 'a', 'b', 'q')")
            .contains("Unrecognized Regex option q")
    );
}

/// A string that will not read as the other type raises, rather than quietly comparing as text and
/// answering false.
#[test]
fn a_string_that_is_not_the_other_type_is_a_conversion_error() {
    let db = database();
    assert!(failure(&db, "SELECT 1 = 'abc'").contains("abc"));
    assert!(failure(&db, "SELECT CAST('2013-07-15' AS DATE) = 'nope'").contains("nope"));
}

#[test]
fn a_cast_that_cannot_hold_the_value_is_an_error_and_try_cast_is_null() {
    let db = database();
    assert!(failure(&db, "SELECT CAST('oops' AS INTEGER)").contains("oops"));
    assert_eq!(rows(&db, "SELECT TRY_CAST('oops' AS INTEGER)"), vec![vec![Value::Null]]);
}

#[test]
fn a_missing_table_names_the_table() {
    let db = database();
    assert!(failure(&db, "SELECT * FROM nope").contains("nope"));
}

#[test]
fn a_missing_column_names_the_column() {
    let db = database();
    assert!(failure(&db, "SELECT nope FROM t").contains("nope"));
}

#[test]
fn a_syntax_error_is_a_parser_error_rather_than_a_panic() {
    let db = database();
    let error = db.query("SELECT FROM WHERE").unwrap_err();
    assert_eq!(error.code(), rudb_common::ErrorCode::Parser);
}

/// The convenience path, and the check that it refuses anything that is not one cell.
#[test]
fn value_returns_one_cell_and_refuses_anything_else() {
    let db = database();
    assert_eq!(db.value("SELECT count(*) FROM t").unwrap(), Value::BigInt(4));
    assert!(db.value("SELECT * FROM t").is_err());
}

/// The plan text is the interface the optimizer tests are written against, so it is worth one test
/// here that a bound query prints in the form `rudb_plan` parses.
#[test]
fn a_plan_prints_parent_before_child() {
    let db = database();
    let text = db.plan("SELECT x FROM t WHERE x > 1").unwrap();
    let lines: Vec<&str> = text.lines().collect();
    assert!(lines[0].starts_with("Project"), "{text}");
    assert!(lines[1].trim_start().starts_with("Filter"), "{text}");
    assert!(lines[2].trim_start().starts_with("Get memory.main.t"), "{text}");
}

#[test]
fn creating_a_table_twice_is_an_error_and_dropping_it_makes_room_again() {
    let db = Database::new();
    db.create_table("t", vec![Field::new("x", LogicalType::Integer)]).unwrap();
    assert!(db.create_table("t", vec![Field::new("x", LogicalType::Integer)]).is_err());
    db.drop_table("t").unwrap();
    db.create_table("t", vec![Field::new("x", LogicalType::Integer)]).unwrap();
}

/// More rows than one vector holds, so the result is more than one chunk and the row indexing has
/// to walk across the boundary.
#[test]
fn a_result_wider_than_one_vector_reads_across_chunks() {
    let db = Database::new();
    db.create_table("big", vec![Field::new("x", LogicalType::Integer)]).unwrap();
    let mut rows = Vec::new();
    for x in 0..2_500 {
        rows.push(vec![Value::Integer(x)]);
    }
    db.append("big", &rows).unwrap();

    let result = db.query("SELECT x FROM big").unwrap();
    assert_eq!(result.len(), 2_500);
    assert!(result.chunks().len() > 1);
    assert_eq!(result.value_at(0, 0), integer(0));
    assert_eq!(result.value_at(1_500, 0), integer(1_500));
    assert_eq!(result.value_at(2_499, 0), integer(2_499));
    assert_eq!(result.row(2_500), None);
    assert_eq!(result.value_at(2_500, 0), Value::Null);
    assert_eq!(db.value("SELECT count(*) FROM big").unwrap(), Value::BigInt(2_500));
}

#[test]
fn a_table_can_be_named_with_its_schema_and_its_catalog() {
    let db = database();
    assert_eq!(db.value("SELECT count(*) FROM main.t").unwrap(), Value::BigInt(4));
    assert_eq!(db.value("SELECT count(*) FROM memory.main.t").unwrap(), Value::BigInt(4));
    assert_eq!(db.table_len("main.t").unwrap(), 4);
}

// The statements that write. Everything above builds its tables through the Rust API, which is
// still the way a program embedded in something else does it. These build them through SQL, which
// is what the sqllogictest corpus needs and what a person at a prompt does.

/// A database built entirely out of SQL.
fn scripted(statements: &[&str]) -> Database {
    let db = Database::new();
    for statement in statements {
        db.execute(statement).unwrap_or_else(|error| panic!("{statement}: {error}"));
    }
    db
}

/// The message a statement fails with.
fn refusal(db: &Database, sql: &str) -> String {
    db.execute(sql).unwrap_err().message().to_string()
}

#[test]
fn a_table_can_be_created_filled_and_read_without_leaving_sql() {
    let db = scripted(&[
        "CREATE TABLE t (a INTEGER, b VARCHAR)",
        "INSERT INTO t VALUES (1, 'one'), (2, 'two')",
    ]);
    assert_eq!(
        rows(&db, "SELECT a, b FROM t"),
        vec![vec![integer(1), text("one")], vec![integer(2), text("two")],]
    );
}

#[test]
fn a_value_is_cast_to_the_column_it_lands_in() {
    // The literal is a `TINYINT` the way it is written and the column is a `BIGINT`, and the cast
    // that reconciles them is in the plan rather than in the append, so the column holds one type.
    let db = scripted(&["CREATE TABLE t (a BIGINT, b DOUBLE)", "INSERT INTO t VALUES (1, 2)"]);
    assert_eq!(rows(&db, "SELECT a, b FROM t"), vec![vec![Value::BigInt(1), Value::Double(2.0)]]);
}

#[test]
fn a_column_the_insert_did_not_name_is_null() {
    let db = scripted(&[
        "CREATE TABLE t (a INTEGER, b VARCHAR, c INTEGER)",
        "INSERT INTO t (c, a) VALUES (30, 10)",
    ]);
    // The list also says the order, so the thirty is in `c` and the ten is in `a`.
    assert_eq!(
        rows(&db, "SELECT a, b, c FROM t"),
        vec![vec![integer(10), Value::Null, integer(30)]]
    );
}

#[test]
fn an_insert_that_reads_its_own_target_sees_the_rows_that_were_there_when_it_started() {
    let db = scripted(&[
        "CREATE TABLE t (a INTEGER)",
        "INSERT INTO t VALUES (1), (2)",
        "INSERT INTO t SELECT a + 10 FROM t",
    ]);
    assert_eq!(
        rows(&db, "SELECT a FROM t"),
        vec![vec![integer(1)], vec![integer(2)], vec![integer(11)], vec![integer(12)],]
    );
}

#[test]
fn create_table_as_takes_its_types_from_the_query() {
    let db = scripted(&[
        "CREATE TABLE t (a INTEGER)",
        "INSERT INTO t VALUES (1), (2), (3)",
        "CREATE TABLE counted AS SELECT count(*) AS n, sum(a) AS total FROM t",
    ]);
    assert_eq!(
        db.query("SELECT n, total FROM counted").unwrap().types(),
        &[LogicalType::BigInt, LogicalType::HugeInt]
    );
    assert_eq!(
        rows(&db, "SELECT n, total FROM counted"),
        vec![vec![Value::BigInt(3), Value::HugeInt(6)]]
    );
}

#[test]
fn a_create_table_as_can_rename_the_query_s_columns() {
    let db = scripted(&["CREATE TABLE t (x, y) AS SELECT 1, 'a'"]);
    assert_eq!(db.query("SELECT * FROM t").unwrap().names(), &["x", "y"]);
}

#[test]
fn a_short_column_list_renames_the_front_and_leaves_the_rest_to_the_query() {
    // Not an error. duckdb v1.4.1 makes a table of `a` and `2` here, where the second name is what
    // the query calls that column, which for a bare literal is the literal.
    let db = scripted(&["CREATE TABLE t (a) AS SELECT 1, 2"]);
    assert_eq!(db.query("SELECT * FROM t").unwrap().names(), &["a", "2"]);
}

#[test]
fn a_column_list_longer_than_the_query_is_the_error_duckdb_writes_for_it() {
    let db = Database::new();
    assert_eq!(
        refusal(&db, "CREATE TABLE t (a, b, c) AS SELECT 1, 2"),
        "Target table has more colum names than query result."
    );
}

#[test]
fn a_query_that_names_two_columns_the_same_gets_them_renamed_apart() {
    // A query may produce two columns of one name and `SELECT 1 AS a, 2 AS a` prints both, so
    // turning one into a table has to decide, and DuckDB renames rather than refusing.
    let db = scripted(&["CREATE TABLE t AS SELECT 1 AS a, 2 AS a, 3 AS a"]);
    assert_eq!(db.query("SELECT * FROM t").unwrap().names(), &["a", "a_1", "a_2"]);
}

#[test]
fn a_renamed_column_steps_past_a_name_the_query_already_used() {
    let db = scripted(&["CREATE TABLE t AS SELECT 1 AS a, 2 AS a, 3 AS a_1"]);
    assert_eq!(db.query("SELECT * FROM t").unwrap().names(), &["a", "a_1", "a_1_1"]);
    let db = scripted(&["CREATE TABLE t AS SELECT 1 AS a_1, 2 AS a, 3 AS a"]);
    assert_eq!(db.query("SELECT * FROM t").unwrap().names(), &["a_1", "a", "a_2"]);
}

#[test]
fn the_renaming_is_case_insensitive_and_keeps_the_case_it_was_written_in() {
    let db = scripted(&["CREATE TABLE t AS SELECT 1 AS a, 2 AS A"]);
    assert_eq!(db.query("SELECT * FROM t").unwrap().names(), &["a", "A_1"]);
}

#[test]
fn a_column_list_turns_the_renaming_off_and_a_repeat_becomes_an_error() {
    // Which is the rule duckdb v1.4.1 follows: with a list, even a short one, the names are the
    // ones written or the ones the query gave, and two the same is a refusal.
    let db = scripted(&["CREATE TABLE t (z) AS SELECT 1 AS a, 2 AS a"]);
    assert_eq!(db.query("SELECT * FROM t").unwrap().names(), &["z", "a"]);
    let db = Database::new();
    assert_eq!(
        refusal(&db, "CREATE TABLE t (z) AS SELECT 1 AS a, 2 AS a, 3 AS a"),
        "Column with name a already exists!"
    );
    let db = Database::new();
    assert_eq!(
        refusal(&db, "CREATE TABLE t (a, a) AS SELECT 1, 2"),
        "Column with name a already exists!"
    );
}

#[test]
fn two_columns_of_one_name_in_a_plain_create_is_the_same_error() {
    let db = Database::new();
    assert_eq!(
        refusal(&db, "CREATE TABLE t (Abc INTEGER, aBC VARCHAR)"),
        "Column with name aBC already exists!"
    );
}

#[test]
fn if_not_exists_leaves_the_table_and_its_rows_alone() {
    let db = scripted(&[
        "CREATE TABLE t (a INTEGER)",
        "INSERT INTO t VALUES (1)",
        "CREATE TABLE IF NOT EXISTS t (b VARCHAR, c VARCHAR)",
    ]);
    assert_eq!(db.query("SELECT * FROM t").unwrap().names(), &["a"]);
    assert_eq!(db.table_len("t").unwrap(), 1);
    // Without it, the second create is an error and the table is still the first one.
    assert!(refusal(&db, "CREATE TABLE t (b VARCHAR)").contains("already exists"));
    assert_eq!(db.query("SELECT * FROM t").unwrap().names(), &["a"]);
}

#[test]
fn or_replace_runs_the_query_against_the_table_it_is_about_to_replace() {
    let db = scripted(&[
        "CREATE TABLE t (a INTEGER)",
        "INSERT INTO t VALUES (1), (2), (3)",
        "CREATE OR REPLACE TABLE t AS SELECT a * 2 AS a FROM t",
    ]);
    assert_eq!(
        rows(&db, "SELECT a FROM t"),
        vec![vec![integer(2)], vec![integer(4)], vec![integer(6)],]
    );
}

#[test]
fn dropping_takes_a_list_and_if_exists_forgives_a_name_that_is_not_there() {
    let db = scripted(&[
        "CREATE TABLE a (x INTEGER)",
        "CREATE TABLE b (x INTEGER)",
        "DROP TABLE a, b",
        "DROP TABLE IF EXISTS a",
    ]);
    assert!(db.with_catalog(|catalog| catalog.tables().next().is_none()));
    assert!(refusal(&db, "DROP TABLE a").contains("does not exist"));
}

#[test]
fn values_is_a_query_and_the_columns_take_the_type_every_row_agrees_on() {
    let db = Database::new();
    let result = db.query("VALUES (1, 'a'), (2.5, 'b')").unwrap();
    assert_eq!(result.names(), &["col0", "col1"]);
    // The integer column widens to hold the integer's digits as well as the fraction, which is
    // `promote_numeric`'s rule and the reason it is eleven wide and not two.
    assert_eq!(
        result.types(),
        &[LogicalType::Decimal { width: 11, scale: 1 }, LogicalType::Varchar]
    );
    assert_eq!(result.len(), 2);
    assert_eq!(
        rows(&db, "SELECT col0 FROM (VALUES (3), (1), (2)) ORDER BY col0"),
        vec![vec![integer(1)], vec![integer(2)], vec![integer(3)]]
    );
}

#[test]
fn a_statement_that_writes_something_the_answer_would_depend_on_is_refused() {
    // Each of these parses and each of them would be a wrong answer if it were accepted and the
    // clause ignored, which is the rule the front end follows everywhere else.
    let db = scripted(&["CREATE TABLE t (a INTEGER)"]);
    for statement in [
        "CREATE TEMPORARY TABLE u (a INTEGER)",
        "CREATE TABLE u (a INTEGER PRIMARY KEY)",
        "INSERT INTO t VALUES (1) RETURNING a",
        "INSERT INTO t (a, a) VALUES (1, 2)",
        "CREATE TABLE u (a INTEGER, a VARCHAR)",
    ] {
        let message = refusal(&db, statement);
        assert!(!message.is_empty(), "{statement} was accepted");
    }
    assert!(db.with_catalog(|catalog| catalog.tables().all(|table| table.name().table != "u")));
}

#[test]
fn a_not_null_column_refuses_a_null_and_keeps_what_came_before_it() {
    let db = scripted(&[
        "CREATE TABLE t (a INTEGER NOT NULL, b VARCHAR)",
        "INSERT INTO t VALUES (1, NULL)",
    ]);
    assert_eq!(refusal(&db, "INSERT INTO t VALUES (NULL, 'x')"), "NOT NULL constraint failed: t.a");
    // The whole statement is refused rather than the row, so the table is what it was before it.
    assert_eq!(db.table_len("t").unwrap(), 1);
    assert_eq!(rows(&db, "SELECT a, b FROM t"), vec![vec![integer(1), Value::Null]]);
}

#[test]
fn a_table_function_produces_rows_where_a_table_would() {
    let db = Database::new();
    assert_eq!(
        rows(&db, "SELECT * FROM range(3)"),
        vec![vec![Value::BigInt(0)], vec![Value::BigInt(1)], vec![Value::BigInt(2)]]
    );
    assert_eq!(
        rows(&db, "SELECT * FROM generate_series(3)"),
        vec![
            vec![Value::BigInt(0)],
            vec![Value::BigInt(1)],
            vec![Value::BigInt(2)],
            vec![Value::BigInt(3)],
        ]
    );
}

#[test]
fn the_column_is_called_what_the_function_is_called_until_it_is_aliased() {
    let db = Database::new();
    let result = db.query("SELECT * FROM range(2)").unwrap();
    assert_eq!(result.names()[0], "range");
    let result = db.query("SELECT i FROM range(2) t(i)").unwrap();
    assert_eq!(result.names()[0], "i");
    // The table alias without a column list renames the table and not the column, which is what
    // makes t.range legal here and t.i not.
    let result = db.query("SELECT t.range FROM range(2) t").unwrap();
    assert_eq!(result.names()[0], "range");
}

#[test]
fn a_table_function_joins_and_aggregates_like_anything_else_in_a_from_clause() {
    let db = database();
    assert_eq!(rows(&db, "SELECT count(*) FROM range(10)"), vec![vec![Value::BigInt(10)]]);
    assert_eq!(rows(&db, "SELECT sum(range) FROM range(1, 5)"), vec![vec![Value::HugeInt(10)]]);
    assert_eq!(
        rows(&db, "SELECT count(*) FROM t, range(3)"),
        vec![vec![Value::BigInt(12)]],
        "four rows against three is twelve"
    );
    assert_eq!(
        rows(&db, "SELECT x FROM t JOIN range(2) ON t.x = range ORDER BY x"),
        vec![vec![integer(1)], vec![integer(1)]]
    );
}

#[test]
fn the_arguments_are_expressions_and_they_cannot_see_a_column() {
    let db = database();
    assert_eq!(rows(&db, "SELECT count(*) FROM range(2 + 3)"), vec![vec![Value::BigInt(5)]]);
    // `FROM t, range(t.x)` is LATERAL, which is a different node and is not bound yet. Resolving
    // the name against whatever is to the left would make the answer depend on the order the two
    // sources were written in.
    let message = failure(&db, "SELECT count(*) FROM t, range(t.x)");
    assert!(message.contains("not found in FROM clause"), "{message}");
}

#[test]
fn a_table_function_that_does_not_exist_says_so_rather_than_being_read_as_a_table() {
    let db = Database::new();
    // `read_json` is the next file reader people will write and it is not one of the four, so it is
    // the one that has to come back named rather than being taken for a table.
    let message = failure(&db, "SELECT * FROM read_json('x.json')");
    assert!(message.contains("read_json"), "{message}");
    let message = failure(&db, "SELECT * FROM nowhere.range(3)");
    assert!(message.contains("nowhere"), "{message}");
    let message = failure(&db, "SELECT * FROM range(1, 2, 3, 4)");
    assert!(message.contains("range"), "{message}");
}

#[test]
fn a_step_of_zero_is_the_one_call_that_is_an_error_rather_than_an_empty_result() {
    let db = Database::new();
    let message = failure(&db, "SELECT * FROM range(1, 5, 0)");
    assert!(message.contains("interval cannot be 0"), "{message}");
    assert!(rows(&db, "SELECT * FROM range(5, 1)").is_empty());
    assert!(rows(&db, "SELECT * FROM range(NULL)").is_empty());
}

#[test]
fn a_range_wider_than_one_chunk_comes_out_whole_and_in_order() {
    // Three thousand crosses the vector boundary, so this is the test that the chunking does not
    // repeat a value or drop one at the seam.
    let db = Database::new();
    let result = db.query("SELECT count(*), min(range), max(range) FROM range(3000)").unwrap();
    let row = result.rows().next().unwrap();
    assert_eq!(row, vec![Value::BigInt(3000), Value::BigInt(0), Value::BigInt(2999)]);
}
