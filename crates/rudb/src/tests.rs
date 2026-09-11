//! End to end tests, from a string of SQL to rows.
//!
//! The layers below have their own tests and this file does not repeat them. `rudb-parse` proves
//! the grammar, `rudb-bind` proves that a query binds to the plan it should, and `rudb-exec` proves
//! that a plan produces the right rows. What is only testable here is the three of them agreeing:
//! a name a query writes has to be the name the binder resolves and the name the executor reads,
//! and a type the binder decided has to be the type the operator produces.

use std::time::Duration;

use rudb_common::{Field, LogicalType, Value, days_from_civil};

use crate::{Config, Database, arrow};

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

/// An order by with a limit runs as a top N, which holds the rows that could still come out rather
/// than the whole input. What it has to produce is what the sort produced, over enough rows that it
/// trims several times and over more than one chunk of input.
#[test]
fn an_order_by_with_a_limit_answers_what_the_sort_would_have() {
    let db = Database::new();
    db.create_table("many", vec![Field::new("x", LogicalType::Integer)]).unwrap();
    // Counting down, so the answer is at the end of the input and nothing is right by accident.
    let counted: Vec<Vec<Value>> = (0..5000).rev().map(|x| vec![Value::Integer(x)]).collect();
    db.append("many", &counted).unwrap();
    let wanted: Vec<Vec<Value>> = (7..17).map(|x| vec![Value::Integer(x)]).collect();
    assert_eq!(rows(&db, "SELECT x FROM many ORDER BY x LIMIT 10 OFFSET 7"), wanted);
    db.execute("SET disabled_optimizers = 'top_n'").unwrap();
    assert_eq!(rows(&db, "SELECT x FROM many ORDER BY x LIMIT 10 OFFSET 7"), wanted);
}

/// The group key of a row is written into the buffer the row before it used, which for a string
/// column means the bytes go where the last row's bytes were rather than into a new allocation. The
/// case that breaks a buffer being reused is a column where what is in the slot and what is arriving
/// keep changing shape, so this one alternates strings of different lengths with nulls and with the
/// empty string, and it runs over enough rows to cross several chunks.
#[test]
fn grouping_a_string_column_counts_each_string_once_however_the_rows_are_ordered() {
    let db = Database::new();
    db.create_table("words", vec![Field::new("s", LogicalType::Varchar)]).unwrap();
    let shapes = [Some(""), Some("a"), None, Some("a longer one"), None, Some("ab")];
    let written: Vec<Vec<Value>> = (0..6000)
        .map(|at| match shapes[at % shapes.len()] {
            Some(word) => vec![Value::Varchar(word.to_string())],
            None => vec![Value::Null],
        })
        .collect();
    db.append("words", &written).unwrap();
    let mut answer = rows(&db, "SELECT s, count(*) FROM words GROUP BY s");
    answer.sort_by_key(|row| format!("{:?}", row[0]));
    assert_eq!(
        answer,
        vec![
            vec![Value::Null, Value::BigInt(2000)],
            vec![text(""), Value::BigInt(1000)],
            vec![text("a longer one"), Value::BigInt(1000)],
            vec![text("a"), Value::BigInt(1000)],
            vec![text("ab"), Value::BigInt(1000)],
        ]
    );
    assert_eq!(
        rows(&db, "SELECT count(DISTINCT s) FROM words"),
        vec![vec![Value::BigInt(4)]],
        "a distinct inside an aggregate keys on the same buffer"
    );
    assert_eq!(
        rows(&db, "SELECT count(*) FROM (SELECT DISTINCT s FROM words)"),
        vec![vec![Value::BigInt(5)]],
        "and duplicate elimination counts the null group as a row where count(DISTINCT) does not"
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
fn counting_a_distinct_counts_the_distinct_values() {
    // The one wrong answer column pruning can produce. Nothing above the `DISTINCT` names a column,
    // and a plain `DISTINCT` names none either, so the pass that narrows a projection to what is
    // read of it used to leave an operator deduplicating rows with nothing in them, and this came
    // back as 1 on any table with a row in it. `crates/rudb-opt/src/columns.rs` is where it lives.
    let db = database();
    assert_eq!(
        rows(&db, "SELECT count(*) FROM (SELECT DISTINCT x FROM t)"),
        vec![vec![Value::BigInt(3)]]
    );
    assert_eq!(
        rows(&db, "SELECT count(*) FROM (SELECT DISTINCT x, s FROM t)"),
        vec![vec![Value::BigInt(4)]]
    );
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

/// The ClickBench entry's own load recipe, over a column that holds what the Parquet holds.
///
/// `hits.parquet` stores the day as days since the epoch and the time as seconds since the epoch,
/// both as integers, and every query in the published set reads a date and a timestamp. DuckDB's
/// entry closes that with `make_date` and `epoch_ms`, so this is those two over the integers the
/// real file has in it, followed by the thing the queries then do with them.
#[test]
fn the_integers_the_benchmark_stores_become_the_dates_the_benchmark_queries() {
    let db = Database::new();
    db.create_table(
        "hits",
        vec![
            Field::new("EventDate", LogicalType::Integer),
            Field::new("EventTime", LogicalType::BigInt),
        ],
    )
    .unwrap();
    let day = i64::from(days_from_civil(2013, 7, 15));
    db.append(
        "hits",
        &[
            vec![
                Value::Integer(days_from_civil(2013, 7, 15)),
                Value::BigInt(day * 86_400 + 37_425),
            ],
            vec![Value::Integer(days_from_civil(2013, 7, 2)), Value::BigInt(day * 86_400 + 37_387)],
        ],
    )
    .unwrap();
    assert_eq!(
        rows(
            &db,
            "SELECT make_date(EventDate) AS d FROM hits WHERE make_date(EventDate) >= '2013-07-10' \
             ORDER BY d"
        ),
        vec![vec![Value::Date(days_from_civil(2013, 7, 15))]]
    );
    assert_eq!(
        rows(
            &db,
            "SELECT extract(minute FROM epoch_ms(EventTime * 1000)) AS m, COUNT(*) FROM hits \
             GROUP BY m ORDER BY m"
        ),
        vec![vec![Value::BigInt(23), Value::BigInt(2)]]
    );
}

/// `SELECT * REPLACE`, which is the other half of that recipe.
///
/// The entry loads the file with one `SELECT * REPLACE (...)` over `read_parquet` rather than by
/// listing all hundred and five columns, so the star has to keep every column it stood for, in
/// order, with the replaced ones substituted in place. The name comes from the replace list and not
/// from the table, which only shows when the two are spelled with different case.
#[test]
fn a_star_can_replace_some_of_what_it_stands_for() {
    let db = Database::new();
    db.create_table(
        "hits",
        vec![
            Field::new("EventDate", LogicalType::Integer),
            Field::new("UserID", LogicalType::BigInt),
        ],
    )
    .unwrap();
    db.append("hits", &[vec![Value::Integer(days_from_civil(2013, 7, 15)), Value::BigInt(7)]])
        .unwrap();
    let result =
        db.query("SELECT * REPLACE (make_date(EventDate) AS eventdate) FROM hits").unwrap();
    assert_eq!(result.names(), &["eventdate", "UserID"]);
    assert_eq!(
        result.rows().collect::<Vec<_>>(),
        vec![vec![Value::Date(days_from_civil(2013, 7, 15)), Value::BigInt(7)]]
    );
    // A qualified star takes one too, and the replacement is an ordinary expression that can read
    // any column in scope and not only the one it is replacing.
    assert_eq!(
        rows(&db, "SELECT hits.* REPLACE (UserID + 1 AS UserID) FROM hits"),
        vec![vec![Value::Integer(days_from_civil(2013, 7, 15)), Value::BigInt(8)]]
    );
    assert_eq!(
        failure(&db, "SELECT * REPLACE (nope + 1 AS nope) FROM hits"),
        "Column \"nope\" in REPLACE list not found in FROM clause Candidate bindings: \
         \"EventDate\", \"UserID\""
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

#[test]
fn a_result_converts_to_arrow_columns_with_the_names_the_query_gave_them() {
    let db = database();
    let result = db.query("SELECT x, s FROM t ORDER BY x, s").unwrap();
    let batches = result.to_arrow().unwrap();
    assert_eq!(batches.len(), result.chunk_count());
    let batch = &batches[0];
    assert_eq!(batch.len(), 4);
    assert_eq!(
        batch.schema().fields,
        vec![
            arrow::Field::new("x", arrow::DataType::Int32),
            arrow::Field::new("s", arrow::DataType::Utf8)
        ]
    );
    // The rows sort to 1 a, 1 null, 2 c, 3 a, so the integers are four little endian words and the
    // strings are the three non null ones back to back with the null taking no bytes.
    assert_eq!(
        batch.column(0).unwrap().values(),
        &[1, 0, 0, 0, 1, 0, 0, 0, 2, 0, 0, 0, 3, 0, 0, 0]
    );
    assert_eq!(batch.column(1).unwrap().values(), b"aca");
    assert_eq!(batch.column(1).unwrap().null_count(), 1);
}

#[test]
fn a_query_that_produced_no_rows_still_says_what_its_columns_were() {
    let db = database();
    let result = db.query("SELECT x FROM empty").unwrap();
    assert!(result.to_arrow().unwrap().is_empty());
    // Which is exactly why the schema is a separate call. A consumer reading it off the first batch
    // would have no batch to read it off, and the query still has a column and that column still
    // has a type.
    let schema = result.arrow_schema().unwrap();
    assert_eq!(schema.fields, vec![arrow::Field::new("x", arrow::DataType::Int32)]);
    let empty = arrow::RecordBatch::empty(schema).unwrap();
    assert_eq!(empty.width(), 1);
    assert!(empty.is_empty());
}

#[test]
fn an_aggregate_over_a_wide_result_keeps_every_chunk_as_its_own_batch() {
    // Batches are per chunk rather than one for the whole result, so a result that crossed the
    // vector boundary is the case that proves the rows are all there and none of them are twice.
    let db = Database::new();
    let result = db.query("SELECT range FROM range(3000)").unwrap();
    let batches = result.to_arrow().unwrap();
    assert!(batches.len() > 1, "3000 rows is more than one chunk");
    let rows: usize = batches.iter().map(rudb_arrow::RecordBatch::len).sum();
    assert_eq!(rows, 3000);
    let bytes: usize = batches.iter().map(|batch| batch.column(0).unwrap().values().len()).sum();
    assert_eq!(bytes, 3000 * 8);
}

#[test]
fn a_database_remembers_what_it_was_opened_with() {
    let config = Config::new()
        .with_memory_limit_text("2GB")
        .unwrap()
        .with_threads(3)
        .unwrap()
        .with_query_timeout(Duration::from_secs(10));
    let db = Database::open_with(":memory:", config).unwrap();
    assert_eq!(db.config().memory_limit(), Some(2_000_000_000));
    assert_eq!(db.config().threads(), 3);
    assert_eq!(db.config().query_timeout(), Some(Duration::from_secs(10)));
    // The settings survive the query path, which is the whole point of reading them back: a harness
    // that opened the database is the thing that reports what the run used.
    db.query("SELECT 1").unwrap();
    assert_eq!(db.config().threads(), 3);
}

#[test]
fn a_database_opened_the_plain_way_has_the_defaults() {
    let db = Database::new();
    assert_eq!(db.config(), Config::default());
    // Eighty percent of the machine, so the number belongs to the machine and what is asserted is
    // that the database took it. The one it must not be is no limit, since that is the state where
    // a runaway query is stopped by the allocator and takes the process with it. See #219.
    assert_eq!(db.config().memory_limit(), rudb_io::default_memory_limit());
    assert_eq!(db.memory().limit(), db.config().memory_limit());
}

/// The query that runs long enough to be stopped, which is a count nobody waits for.
const FOREVER: &str = "SELECT count(*) FROM range(100000000000)";

#[test]
fn a_query_that_runs_too_long_is_stopped_by_the_limit_it_was_opened_with() {
    let db = Database::with_config(Config::new().with_query_timeout(Duration::from_millis(50)));
    let started = std::time::Instant::now();
    let error = db.query(FOREVER).expect_err("that does not finish");
    assert_eq!(error.code().duckdb_name(), "Interrupt Error");
    assert!(error.message().contains("50 millisecond"), "{error}");
    // The limit is on the statement rather than a suggestion, so this has to be over in about the
    // time it was given rather than in the time the query would have taken, which is hours.
    assert!(started.elapsed() < Duration::from_secs(10), "{:?}", started.elapsed());
}

#[test]
fn a_limit_does_not_stop_a_query_that_finishes_inside_it() {
    let db = Database::with_config(Config::new().with_query_timeout(Duration::from_secs(60)));
    assert_eq!(db.value("SELECT 42").unwrap(), Value::Integer(42));
    // And the clock starts again for the next statement rather than carrying on from the first.
    assert_eq!(db.value("SELECT 43").unwrap(), Value::Integer(43));
}

#[test]
fn another_thread_can_interrupt_a_running_query() {
    let db = Database::new();
    let connection = db.connect();
    let stopper = connection.clone();
    let watchdog = std::thread::spawn(move || {
        std::thread::sleep(Duration::from_millis(50));
        stopper.interrupt();
    });
    let error = connection.query(FOREVER).expect_err("the other thread stopped it");
    watchdog.join().expect("the watchdog ran");
    assert_eq!(error.code().duckdb_name(), "Interrupt Error");
    assert_eq!(error.message(), "Interrupted!");
}

#[test]
fn an_interrupt_stops_the_statement_it_was_meant_for_and_not_the_next_one() {
    let db = Database::new();
    let connection = db.connect();
    connection.interrupt();
    // The flag is cleared at the top of each statement, so an interrupt nothing was running under
    // is dropped rather than killing whatever comes next.
    assert_eq!(connection.value("SELECT 1").unwrap(), Value::Integer(1));
}

#[test]
fn a_statement_that_writes_is_stoppable_too_and_leaves_nothing_behind() {
    let db = Database::with_config(Config::new().with_query_timeout(Duration::from_millis(50)));
    db.execute("CREATE TABLE big (x BIGINT)").unwrap();
    let error = db
        .execute("INSERT INTO big SELECT x FROM range(100000000000) t(x)")
        .expect_err("that does not finish");
    assert_eq!(error.code().duckdb_name(), "Interrupt Error");
    // Nothing was appended, because the source runs to completion before anything is. That is not
    // a rollback, it is the absence of a partial write, and it stops being enough the day the
    // writes stream.
    assert_eq!(db.table_len("big").unwrap(), 0);
}

/// A memory limit small enough that a sort over a few hundred thousand rows cannot fit in it.
///
/// A row buffered by a pipeline breaker is a `Vec<Value>` today, which is a hundred bytes or so for
/// one number, so a megabyte is a few thousand rows. The queries below are far larger than that, so
/// the test is about the limit stopping them and not about where exactly the boundary sits.
const SMALL: u64 = 1 << 20;

#[test]
fn a_query_that_buffers_too_much_is_stopped_by_the_memory_limit() {
    let db = Database::with_config(Config::new().with_memory_limit(SMALL));
    let error = db.query("SELECT * FROM range(10000000) ORDER BY range").expect_err("too large");
    assert_eq!(error.code().duckdb_name(), "Out of Memory Error");
    assert!(error.message().contains("could not allocate"), "{error}");
    assert!(error.message().contains("1.0 MiB used"), "{error}");
}

#[test]
fn every_operator_that_buffers_is_held_to_the_limit() {
    // One query per pipeline breaker, because a limit that catches the sort and not the grouping is
    // a limit somebody finds out about from a dead process rather than from an error.
    let queries = [
        "SELECT * FROM range(10000000) ORDER BY range",
        "SELECT range, count(*) FROM range(10000000) GROUP BY range",
        "SELECT DISTINCT range FROM range(10000000)",
        "SELECT * FROM range(10000000) UNION ALL SELECT * FROM range(10)",
        "SELECT * FROM range(10000000) a, range(10000000) b WHERE a.range = b.range",
    ];
    for query in queries {
        let db = Database::with_config(Config::new().with_memory_limit(SMALL));
        let error = db.query(query).err().unwrap_or_else(|| panic!("{query} should not have fit"));
        assert_eq!(error.code().duckdb_name(), "Out of Memory Error", "{query}");
    }
}

#[test]
fn the_budget_is_given_back_when_the_query_stops() {
    let db = Database::with_config(Config::new().with_memory_limit(SMALL));
    db.query("SELECT * FROM range(10000000) ORDER BY range").expect_err("too large");
    assert_eq!(db.memory().used(), 0, "an operator that failed still gave its rows back");
    // And the database is usable, which is the difference between an error and a dead process.
    assert_eq!(db.value("SELECT 1").unwrap(), Value::Integer(1));
}

#[test]
fn a_result_holds_its_bytes_until_it_is_dropped() {
    let db = Database::with_config(Config::new().with_memory_limit(SMALL));
    let result = db.query("SELECT * FROM range(1000)").unwrap();
    assert!(result.footprint() > 0, "a thousand rows are charged something");
    assert_eq!(db.memory().used(), result.footprint());
    drop(result);
    assert_eq!(db.memory().used(), 0);
}

#[test]
fn a_database_with_no_limit_counts_what_it_holds_anyway() {
    // So that a program can watch the number before it decides what limit to set. It has to ask for
    // no limit now, because a database opened the plain way has one.
    let db = Database::with_config(Config::new().with_no_memory_limit());
    let result = db.query("SELECT * FROM range(1000)").unwrap();
    assert_eq!(db.memory().limit(), None);
    assert_eq!(db.memory().used(), result.footprint());
}

#[test]
fn the_limit_is_on_the_database_rather_than_on_each_query() {
    // Two results alive at once are held to one limit between them, which is what DuckDB's
    // memory_limit means and the only reading that is any use.
    let db = Database::with_config(Config::new().with_memory_limit(SMALL));
    // Eighty thousand eight byte numbers is most of a megabyte and two of them is more than one.
    let wide = "SELECT range FROM range(80000)";
    let first = db.query(wide).expect("one fits");
    let error = db.query(wide).expect_err("there is no room for a second copy");
    assert_eq!(error.code().duckdb_name(), "Out of Memory Error");
    drop(first);
    db.query(wide).expect("the room came back");
}

#[test]
fn set_disabled_optimizers_turns_a_pass_off_for_the_statements_that_follow() {
    // The whole reason this statement exists: a plan that is wrong after optimization and right
    // before it is a plan whose pass can be named, and naming it is how the bisector will work.
    let db = Database::new();
    let folded = db.plan("SELECT 1 + 2").unwrap();
    assert!(folded.contains('3'), "{folded}");
    db.execute("SET disabled_optimizers = 'expression_rewriter'").unwrap();
    let unfolded = db.plan("SELECT 1 + 2").unwrap();
    assert!(unfolded.contains("\"+\""), "{unfolded}");
    assert_eq!(db.setting("disabled_optimizers").unwrap(), "expression_rewriter");
    db.execute("RESET disabled_optimizers").unwrap();
    assert_eq!(db.plan("SELECT 1 + 2").unwrap(), folded);
}

#[test]
fn a_pass_that_nobody_has_is_refused_by_the_statement_that_named_it() {
    let db = Database::new();
    let error = db.execute("SET disabled_optimizers = 'no_such_pass'").unwrap_err();
    assert_eq!(error.code().duckdb_name(), "Parser Error");
    assert_eq!(db.setting("disabled_optimizers").unwrap(), "", "a refused set changed nothing");
}

#[test]
fn set_memory_limit_moves_the_budget_the_queries_after_it_are_held_to() {
    // Opened with no limit so that the reset at the end has one unambiguous thing to go back to.
    let db = Database::with_config(Config::new().with_no_memory_limit());
    assert_eq!(db.memory().limit(), None);
    db.execute("SET memory_limit = '1GiB'").unwrap();
    assert_eq!(db.memory().limit(), Some(1 << 30));
    assert_eq!(db.config().memory_limit(), Some(1 << 30));
    assert_eq!(db.setting("memory_limit").unwrap(), "1.0 GiB");
    // A gigabyte and a gibibyte are two different numbers, which is what the binary does.
    db.execute("SET memory_limit = '1GB'").unwrap();
    assert_eq!(db.memory().limit(), Some(1_000_000_000));
    db.execute("RESET memory_limit").unwrap();
    assert_eq!(db.memory().limit(), None, "back to what the database was opened with");
}

#[test]
fn a_memory_limit_set_in_the_middle_of_a_session_refuses_the_next_query_that_passes_it() {
    let db = Database::new();
    let wide = "SELECT range FROM range(80000)";
    db.query(wide).expect("no limit yet");
    // Eighty thousand eight byte numbers is six hundred and forty kilobytes, so a hundred is not
    // enough room for them and the query that ran a moment ago stops running.
    db.execute("SET memory_limit = '100KB'").unwrap();
    let error = db.query(wide).expect_err("the limit is on now");
    assert_eq!(error.code().duckdb_name(), "Out of Memory Error");
}

#[test]
fn set_threads_is_recorded_even_though_nothing_runs_in_parallel_yet() {
    let db = Database::with_config(Config::new().with_threads(2).unwrap());
    db.execute("SET threads = 8").unwrap();
    assert_eq!(db.config().threads(), 8);
    assert_eq!(db.setting("threads").unwrap(), "8");
    let error = db.execute("SET threads = 0").unwrap_err();
    assert_eq!(error.to_string(), "Syntax Error: Must have at least 1 thread!");
    db.execute("RESET threads").unwrap();
    assert_eq!(db.config().threads(), 2, "reset is what the database was opened with");
}

#[test]
fn a_name_that_is_not_a_setting_says_which_ones_there_are() {
    let db = Database::new();
    let error = db.execute("SET bogus = 1").unwrap_err();
    assert_eq!(error.code().duckdb_name(), "Catalog Error");
    assert!(error.message().contains("\"bogus\""), "{error}");
    assert!(error.message().contains("\"threads\""), "{error}");
}

#[test]
fn the_two_scopes_this_database_does_not_have_are_two_different_sentences() {
    let db = Database::new();
    let error = db.execute("SET LOCAL threads = 2").unwrap_err();
    assert_eq!(error.to_string(), "Not implemented Error: SET LOCAL is not implemented.");
    let error = db.execute("SET SESSION threads = 2").unwrap_err();
    assert_eq!(error.to_string(), "Catalog Error: option \"threads\" cannot be set locally");
    db.execute("SET GLOBAL threads = 2").expect("global is the scope every setting here has");
    assert_eq!(db.config().threads(), 2);
}

#[test]
fn the_settings_a_database_was_opened_with_are_what_reset_goes_back_to() {
    let db = Database::with_config(Config::new().with_memory_limit(SMALL).with_threads(2).unwrap());
    db.execute("SET memory_limit = '4GB'").unwrap();
    db.execute("SET threads = 16").unwrap();
    assert_eq!(db.opened_with().memory_limit(), Some(SMALL));
    db.execute("RESET memory_limit").unwrap();
    db.execute("RESET threads").unwrap();
    assert_eq!(db.config(), db.opened_with());
    assert_eq!(db.memory().limit(), Some(SMALL));
}
