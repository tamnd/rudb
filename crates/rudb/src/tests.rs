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

/// The first column of a row, for a test that sorts a grouped answer by a `BIGINT` key.
fn first_key(row: &[Value]) -> i64 {
    match row[0] {
        Value::BigInt(key) => key,
        ref other => panic!("the key of this query is a BIGINT, not {other:?}"),
    }
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

fn list(values: &[i32]) -> Value {
    Value::List {
        element: LogicalType::Integer,
        values: values.iter().map(|&v| Value::Integer(v)).collect(),
    }
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

/// The compact numeric group state has to keep SUM and AVG nulls independent of COUNT(*).
#[test]
fn grouped_smallint_sum_and_avg_keep_nulls() {
    let db = scripted(&[
        "CREATE TABLE numbers (k INTEGER, a SMALLINT, b SMALLINT)",
        "INSERT INTO numbers VALUES (1, 1, 2), (1, NULL, 4), (1, 2, NULL), \
         (2, NULL, NULL), (2, NULL, NULL)",
    ]);
    assert_eq!(
        rows(&db, "SELECT k, COUNT(*), SUM(a), AVG(b) FROM numbers GROUP BY k ORDER BY k"),
        vec![
            vec![integer(1), Value::BigInt(3), Value::HugeInt(3), Value::Double(3.0)],
            vec![integer(2), Value::BigInt(2), Value::Null, Value::Null],
        ]
    );
}

#[test]
fn an_unordered_limit_keeps_only_the_groups_it_can_return() {
    let db = database();
    let full =
        db.query("SELECT range % 10000 AS k, count(*) FROM range(100000) GROUP BY k").unwrap();
    let full_peak = full.metrics().unwrap().resource.peak_bytes;
    drop(full);
    let limited = db
        .query("SELECT range % 10000 AS k, count(*) FROM range(100000) GROUP BY k LIMIT 10")
        .unwrap();
    assert_eq!(
        limited.rows().collect::<Vec<_>>(),
        (0..10).map(|k| vec![Value::BigInt(k), Value::BigInt(10)]).collect::<Vec<_>>()
    );
    let limited_peak = limited.metrics().unwrap().resource.peak_bytes;
    assert!(limited_peak * 10 < full_peak, "{limited_peak} was not far below {full_peak}");
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

/// The same question asked of a string that came out of a cross product, which is where it was
/// answered wrong. A cross product hands the left row down as constant vectors, one value standing
/// for the whole chunk, and the comparison after the hash probe could not read one, so every row
/// opened a group of its own. TPC-H q05, q07 and q10 all returned the same group key once per row
/// because of it, and they are all written with a comma in the `FROM` rather than a `JOIN`.
#[test]
fn grouping_a_string_that_came_out_of_a_cross_product_still_counts_each_string_once() {
    let db = Database::new();
    db.execute("CREATE TABLE w AS SELECT i AS k, 'word' || (i % 3) AS s FROM range(9) t(i)")
        .unwrap();
    db.execute("CREATE TABLE n AS SELECT i AS k FROM range(9) t(i)").unwrap();
    let mut answer = rows(&db, "SELECT s, count(*) FROM w, n WHERE w.k = n.k GROUP BY s");
    answer.sort_by_key(|row| format!("{:?}", row[0]));
    assert_eq!(
        answer,
        vec![
            vec![text("word0"), Value::BigInt(3)],
            vec![text("word1"), Value::BigInt(3)],
            vec![text("word2"), Value::BigInt(3)],
        ]
    );
    assert_eq!(
        rows(&db, "SELECT count(*) FROM (SELECT DISTINCT s FROM w, n WHERE w.k = n.k)"),
        vec![vec![Value::BigInt(3)]],
        "and duplicate elimination keys on the same table"
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

/// A part of an interval is one of its three fields and never a sum of them, so thirty six hours
/// has no days in it and asking for the days says zero.
///
/// The column form is here as well as the single value one because the two are different loops, and
/// the parts an interval does not have are refused in both of them.
#[test]
fn a_part_of_an_interval_is_one_of_its_three_fields() {
    let db = database();
    let lengths = "FROM (VALUES (INTERVAL '14 months'), (INTERVAL '-14 months'), \
                   (CAST(NULL AS INTERVAL))) t(length)";
    let one = |value: i64| vec![vec![Value::BigInt(value)]];
    assert_eq!(rows(&db, "SELECT date_part('hour', INTERVAL '36 hours')"), one(36));
    assert_eq!(rows(&db, "SELECT date_part('day', INTERVAL '36 hours')"), one(0));
    assert_eq!(rows(&db, "SELECT extract(month FROM INTERVAL '14 months')"), one(2));
    assert_eq!(
        rows(&db, &format!("SELECT date_part('month', length) {lengths}")),
        vec![vec![Value::BigInt(2)], vec![Value::BigInt(-2)], vec![Value::Null]]
    );
    let refused = failure(&db, &format!("SELECT date_part('week', length) {lengths}"));
    assert_eq!(refused, "\"interval\" units \"week\" not recognized");
    let refused = failure(&db, "SELECT date_part('week', INTERVAL '5 days')");
    assert_eq!(refused, "\"interval\" units \"week\" not recognized");
}

/// `age` counts a gap in calendar fields, which is not what the subtraction answers, and a day it is
/// short by comes out of the earlier moment's month.
///
/// A date reaches the function by widening to a timestamp, and a time and an interval do not widen
/// anywhere, so those two are refused here the way they are refused upstream.
#[test]
fn a_gap_in_calendar_fields_is_not_the_same_as_a_difference() {
    let db = database();
    let gap = |months: i32, days: i32| vec![vec![Value::Interval { months, days, micros: 0 }]];
    let late = "TIMESTAMP '2020-07-01'";
    let early = "TIMESTAMP '2020-02-28'";
    assert_eq!(rows(&db, &format!("SELECT age({late}, {early})")), gap(4, 2));
    assert_eq!(rows(&db, &format!("SELECT age({early}, {late})")), gap(-4, -2));
    assert_eq!(rows(&db, &format!("SELECT {late} - {early}")), gap(0, 124));
    assert_eq!(rows(&db, "SELECT age(DATE '2020-07-01', DATE '2020-02-28')"), gap(4, 2));
    let moments = "FROM (VALUES (TIMESTAMP '2020-04-30', TIMESTAMP '2020-03-31'), \
                   (TIMESTAMP '2020-01-01', NULL)) t(late, early)";
    assert_eq!(
        rows(&db, &format!("SELECT age(late, early) {moments}")),
        vec![vec![Value::Interval { months: 0, days: 30, micros: 0 }], vec![Value::Null]]
    );
    let refused = failure(&db, "SELECT age(TIME '10:00:00', TIME '09:00:00')");
    assert!(refused.contains("age(TIME, TIME)"), "{refused}");
}

/// `epoch` and `julian` carry a fraction, so `date_part` is declared as a double and the binder
/// narrows it back to a bigint when the specifier is a literal naming a whole part. Midday is in
/// here because that fraction is half a day to `julian` and half of nothing to a date.
///
/// The last case is the reason the narrowing lives in the binder rather than in the function table.
/// A specifier that is a column cannot be looked at while binding, so the answer is a double even
/// when every row of it happens to say `year`, and DuckDB prints `2020.0` there for the same reason.
#[test]
fn the_two_parts_that_carry_a_fraction_are_doubles() {
    let db = database();
    let stamp = "CAST('2020-01-01 12:00:00' AS TIMESTAMP)";
    let one = |value: f64| vec![vec![Value::Double(value)]];
    assert_eq!(rows(&db, &format!("SELECT date_part('epoch', {stamp})")), one(1_577_880_000.0));
    assert_eq!(rows(&db, &format!("SELECT date_part('julian', {stamp})")), one(2_458_850.5));
    assert_eq!(rows(&db, "SELECT date_part('jd', DATE '2020-01-01')"), one(2_458_850.0));
    assert_eq!(rows(&db, "SELECT date_part('epoch', INTERVAL '1 year')"), one(31_557_600.0));
    assert_eq!(
        rows(&db, &format!("SELECT date_part('year', {stamp})")),
        vec![vec![Value::BigInt(2_020)]]
    );
    let parts = "FROM (VALUES ('year'), ('epoch')) t(part)";
    assert_eq!(
        rows(&db, &format!("SELECT date_part(part, DATE '2020-01-01') {parts}")),
        vec![vec![Value::Double(2_020.0)], vec![Value::Double(1_577_836_800.0)]]
    );
}

/// Truncating an interval keeps the fields above the part and clears the ones below it, and the
/// column form has to agree with the single value one about which those are.
#[test]
fn truncating_an_interval_keeps_the_fields_above_the_part() {
    let db = database();
    let length = "INTERVAL '14 months 10 days 06:07:08.9'";
    let months = Value::Interval { months: 14, days: 0, micros: 0 };
    assert_eq!(rows(&db, &format!("SELECT date_trunc('month', {length})")), vec![vec![months]]);
    let week = Value::Interval { months: 14, days: 7, micros: 0 };
    assert_eq!(rows(&db, &format!("SELECT date_trunc('week', {length})")), vec![vec![week]]);
    let lengths = format!("FROM (VALUES ({length}), (CAST(NULL AS INTERVAL))) t(length)");
    let hour = Value::Interval { months: 14, days: 10, micros: 6 * 3_600 * 1_000_000 };
    assert_eq!(
        rows(&db, &format!("SELECT date_trunc('hour', length) {lengths}")),
        vec![vec![hour], vec![Value::Null]]
    );
    let refused = failure(&db, &format!("SELECT date_trunc('era', {length})"));
    assert_eq!(refused, "Specifier type not implemented for DATETRUNC");
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

/// `trim` was the wrong answer this engine was quietest about, so the rules of its shape are
/// asserted where it was seen rather than only in the transformer. Per #313.
///
/// `trim` itself answers now and that is #314. The other rules of the shape still refuse, and the
/// refusal is the point: a rule that wrote a keyword was stepped through as if it were a precedence
/// level, which left the argument behind as the answer.
#[test]
fn a_function_that_is_not_implemented_does_not_answer_its_own_argument() {
    let db = database();
    let message = failure(&db, "SELECT row(1)");
    assert!(message.contains("not supported yet"), "{message}");
    assert!(message.ends_with("RowExpression"), "{message}");
    assert!(failure(&db, "SELECT length(try('a'))").contains("not supported yet"));
}

/// The four string functions with a grammar rule of their own, end to end. Per #314.
///
/// Every answer below was read off the pinned binary one statement at a time, because the index
/// rules are not guessable from each other. The two worth pointing at are a start of zero, which
/// leaves a window that begins before the string and so answers one character short, and a negative
/// length, which runs the window backwards from the start instead of being empty.
#[test]
fn the_string_keywords_answer_the_way_duckdb_does() {
    let db = database();
    let one = |sql: &str| match rows(&db, sql).as_slice() {
        [row] => row.clone(),
        other => panic!("one row, not {}", other.len()),
    };
    assert_eq!(
        one("SELECT substring('abcdef', 2, 3), substring('abcdef' FROM 2 FOR 3)"),
        vec![text("bcd"), text("bcd")]
    );
    assert_eq!(
        one("SELECT substring('abcdef', 2), substring('abcdef' FOR 3), substr('abcdef', 2, 3)"),
        vec![text("bcdef"), text("abc"), text("bcd")]
    );
    assert_eq!(
        one(
            "SELECT substring('abcdef', 0, 3), substring('abcdef', -1, 3), substring('abcdef', 4, -2)"
        ),
        vec![text("ab"), text("f"), text("bc")]
    );
    assert_eq!(
        one(
            "SELECT substring('abcdef', 10, 3), substring('abcdef', -10, 3), substring('abcdef', -10)"
        ),
        vec![text(""), text(""), text("abcdef")]
    );
    assert_eq!(
        one("SELECT position('c' IN 'abcdef'), strpos('abcdef', 'z'), instr('abcdef', 'abc')"),
        vec![Value::BigInt(3), Value::BigInt(0), Value::BigInt(1)]
    );
    assert_eq!(
        one("SELECT trim('  a  '), trim(BOTH 'x' FROM 'xxaxx'), trim('xyaxy', 'xy')"),
        vec![text("a"), text("a"), text("a")]
    );
    assert_eq!(
        one("SELECT trim(LEADING FROM '  a  '), trim(TRAILING FROM '  a  '), ltrim('xxaxx', 'x')"),
        vec![text("a  "), text("  a"), text("axx")]
    );
    assert_eq!(
        one(
            "SELECT overlay('abcdef' PLACING 'X' FROM 2 FOR 1), overlay('abcdef' PLACING 'XY' FROM 2)"
        ),
        vec![text("aXcdef"), text("aXYdef")]
    );
    assert_eq!(
        one(
            "SELECT overlay('abcdef' PLACING 'XY' FROM 2 FOR 0), overlay('abcdef' PLACING 'XY' FROM 2 FOR -1)"
        ),
        vec![text("aXYbcdef"), text("aXYdef")]
    );
    // A null anywhere is a null answer, and the types are the ones upstream declares.
    assert_eq!(
        one("SELECT substring(NULL, 1, 2), trim(NULL), strpos('a', NULL)"),
        vec![Value::Null, Value::Null, Value::Null]
    );
    assert_eq!(
        one("SELECT typeof(substring('abcdef', 2)), typeof(strpos('a', 'b'))"),
        vec![text("VARCHAR"), text("BIGINT")]
    );
    // Over a column, including the null row.
    assert_eq!(
        rows(&db, "SELECT substring(s, 1, 1) FROM t"),
        vec![vec![text("a")], vec![Value::Null], vec![text("c")], vec![text("a")]]
    );
    // The names upstream gives these columns, which are the lowered calls and not what was written.
    // Three of the four are keywords and are quoted, which is #251, and `ltrim` is not a keyword
    // and is not quoted, which is the same rule and is why the last one looks different.
    assert_eq!(
        db.query("SELECT substring(s FROM 2 FOR 3) FROM t").unwrap().names(),
        &["\"substring\"(s, 2, 3)".to_string()]
    );
    assert_eq!(
        db.query("SELECT position('c' IN s) FROM t").unwrap().names(),
        &["\"position\"(s, 'c')".to_string()]
    );
    assert_eq!(
        db.query("SELECT trim(LEADING FROM s) FROM t").unwrap().names(),
        &["ltrim(s)".to_string()]
    );
    // An index has to be a whole number already, which is upstream's rule rather than a rounding.
    let message = failure(&db, "SELECT substring('abcdef', 2.5, 3)");
    assert!(message.contains("\"substring\"(col0 VARCHAR, col1 BIGINT, col2 BIGINT) -> VARCHAR"));
    assert!(failure(&db, "SELECT trim(123)").contains("\"trim\"(col0 VARCHAR) -> VARCHAR"));
    assert!(failure(&db, "SELECT strpos('abcdef')").contains("strpos(col0 VARCHAR, col1 VARCHAR)"));
}

/// An identifier inside a generated column name is quoted where upstream quotes it. Per #251.
///
/// Every name below was read off the pinned binary. The rule turns out to be two rules and neither
/// of them is the obvious one. A word is quoted when it is a keyword in one of the grammar's
/// classes, so `name` and `action` are quoted and `alias` is not, even though `alias` is in the
/// keyword table, because it is spelled by a rule and belongs to no class. And a word is quoted
/// when it is not one plain ASCII word, so a space, a leading digit and an accent all keep their
/// quotes.
///
/// What is not a reason is case. `UserID` comes back unquoted from upstream even though reading
/// that name again would fold it to `userid`, and that is the case ClickBench is made of.
#[test]
fn a_generated_name_quotes_an_identifier_where_duckdb_quotes_one() {
    let db = Database::new();
    let fields = ["name", "alias", "UserID", "my col", "9x"]
        .iter()
        .map(|name| Field::new(*name, LogicalType::Integer))
        .collect();
    db.create_table("q", fields).unwrap();
    let names = |sql: &str| db.query(sql).unwrap().names().to_vec();
    assert_eq!(names("SELECT min(name) FROM q"), &["min(\"name\")".to_string()]);
    assert_eq!(names("SELECT min(alias) FROM q"), &["min(alias)".to_string()]);
    assert_eq!(names("SELECT min(\"UserID\") FROM q"), &["min(UserID)".to_string()]);
    assert_eq!(names("SELECT min(\"my col\") FROM q"), &["min(\"my col\")".to_string()]);
    assert_eq!(names("SELECT min(\"9x\") FROM q"), &["min(\"9x\")".to_string()]);
    // The column on its own is not a generated name at all. It comes from the catalog, so it is
    // the spelling the table was created with and nothing quotes it.
    assert_eq!(names("SELECT name FROM q"), &["name".to_string()]);
    // The same rule everywhere a name is generated, and not only inside an aggregate.
    assert_eq!(names("SELECT name + 1 FROM q"), &["(\"name\" + 1)".to_string()]);
    assert_eq!(
        names("SELECT CAST(name AS VARCHAR) FROM q"),
        &["CAST(\"name\" AS VARCHAR)".to_string()]
    );
    assert_eq!(names("SELECT name IS NULL FROM q"), &["(\"name\" IS NULL)".to_string()]);
}

/// The five string functions a corpus file reaches for constantly, end to end. Per #330.
///
/// Every answer below was read off the pinned binary, because none of it follows from the names.
/// The three that are worth pinning are `concat` dropping a null instead of propagating it, a
/// negative count to `left` counting from the other end instead of raising, and an empty needle to
/// `replace` changing nothing instead of matching everywhere.
#[test]
fn the_five_string_functions_answer_the_way_duckdb_does() {
    let db = database();
    assert_eq!(rows(&db, "SELECT chr(65), chr(233)"), vec![vec![text("A"), text("é")]]);
    assert_eq!(rows(&db, "SELECT length(chr(0))"), vec![vec![Value::BigInt(1)]]);
    assert_eq!(
        rows(&db, "SELECT left('héllo', 2), right('héllo', 2)"),
        vec![vec![text("hé"), text("lo")]]
    );
    assert_eq!(
        rows(&db, "SELECT left('abc', -1), right('abc', -1), left('abc', 0), right('abc', 99)"),
        vec![vec![text("ab"), text("bc"), text(""), text("abc")]]
    );
    assert_eq!(
        rows(&db, "SELECT replace('abc', 'b', 'x'), replace('aaa', '', 'x')"),
        vec![vec![text("axc"), text("aaa")]]
    );
    // `concat` is above the null rule and the other four are not, which is the one line of this
    // that a reader would otherwise have to go and measure.
    assert_eq!(rows(&db, "SELECT concat('a', 1, NULL)"), vec![vec![text("a1")]]);
    assert_eq!(rows(&db, "SELECT concat(NULL)"), vec![vec![text("")]]);
    assert_eq!(
        rows(&db, "SELECT replace('abc', 'b', NULL), left(NULL, 2), chr(NULL)"),
        vec![vec![Value::Null, Value::Null, Value::Null]]
    );
    // The names upstream gives these columns. Three of the five are keywords and are quoted.
    assert_eq!(
        db.query("SELECT LEFT('abc', 2), chr(65)").unwrap().names(),
        &["\"left\"('abc', 2)".to_string(), "chr(65)".to_string()]
    );
    // A code point that is no code point, and the counts and types the signature refuses. `chr`
    // has one overload upstream and it narrows nothing to reach it.
    assert_eq!(failure(&db, "SELECT chr(55296)"), "Invalid UTF8 Codepoint 55296");
    assert!(failure(&db, "SELECT chr(65.9)").contains("chr(col0 INTEGER) -> VARCHAR"));
    assert!(
        failure(&db, "SELECT left('abc')")
            .contains("\"left\"(col0 VARCHAR, col1 BIGINT) -> VARCHAR")
    );
    assert!(failure(&db, "SELECT concat()").contains("concat(col0 ANY, [ANY...]) -> ANY"));
}

/// The interval literal, end to end, which is a function call by the time anything binds it.
/// Per #360.
///
/// The column names are the argument that this is the same rewrite DuckDB does rather than one that
/// happens to agree on the values, so they are asserted next to the answers rather than separately.
/// Every one of them was read off the pinned binary.
#[test]
fn an_interval_literal_is_the_call_duckdb_rewrites_it_into() {
    let db = database();
    let interval = |months, days, micros| Value::Interval { months, days, micros };
    assert_eq!(
        db.query("SELECT INTERVAL 1 DAY").unwrap().names(),
        &["to_days(CAST(trunc(CAST(1 AS DOUBLE)) AS INTEGER))".to_string()]
    );
    assert_eq!(
        db.query("SELECT INTERVAL 90 SECOND").unwrap().names(),
        &["to_seconds(CAST(90 AS DOUBLE))".to_string()]
    );
    assert_eq!(rows(&db, "SELECT INTERVAL 1 DAY"), vec![vec![interval(0, 1, 0)]]);
    assert_eq!(rows(&db, "SELECT INTERVAL 1 DAYS"), vec![vec![interval(0, 1, 0)]]);
    assert_eq!(rows(&db, "SELECT INTERVAL 2 MONTHS"), vec![vec![interval(2, 0, 0)]]);
    assert_eq!(rows(&db, "SELECT INTERVAL 1 WEEK"), vec![vec![interval(0, 7, 0)]]);
    assert_eq!(rows(&db, "SELECT INTERVAL 1 MILLENNIUM"), vec![vec![interval(12_000, 0, 0)]]);
    assert_eq!(rows(&db, "SELECT INTERVAL (1+1) DAY"), vec![vec![interval(0, 2, 0)]]);
    assert_eq!(rows(&db, "SELECT INTERVAL (-1) DAY"), vec![vec![interval(0, -1, 0)]]);
    // The truncation is in the rewrite rather than in the function, so a day and a half is a day,
    // and the two units that keep their fraction do not go through it at all.
    assert_eq!(rows(&db, "SELECT INTERVAL 1.5 DAY"), vec![vec![interval(0, 1, 0)]]);
    assert_eq!(rows(&db, "SELECT INTERVAL 2.7 SECOND"), vec![vec![interval(0, 0, 2_700_000)]]);
    assert_eq!(rows(&db, "SELECT INTERVAL 1 MILLISECOND"), vec![vec![interval(0, 0, 1_000)]]);
    // Written out by hand it is the same call, which is what the rewrite being a rewrite means.
    assert_eq!(rows(&db, "SELECT to_days(1)"), vec![vec![interval(0, 1, 0)]]);
    assert_eq!(rows(&db, "SELECT to_quarters(5)"), vec![vec![interval(15, 0, 0)]]);
    assert_eq!(rows(&db, "SELECT to_days(NULL)"), vec![vec![Value::Null]]);
    // A count reaches its type by widening, so an INTEGER count of hours and a DECIMAL count of
    // seconds both bind, and a DECIMAL count of days does not, because it would have to narrow.
    assert_eq!(rows(&db, "SELECT to_hours(25)"), vec![vec![interval(0, 0, 90_000_000_000)]]);
    assert_eq!(rows(&db, "SELECT to_seconds(1.5)"), vec![vec![interval(0, 0, 1_500_000)]]);
    assert!(failure(&db, "SELECT to_days(1.7)").contains("to_days(col0 INTEGER) -> INTERVAL"));
    // The seven range forms parse and then refuse in upstream's own words, with the units spelled
    // the canonical way rather than the way they were written.
    assert_eq!(failure(&db, "SELECT INTERVAL 1 DAYS TO HOURS"), "DAY TO HOUR is not supported");
    assert_eq!(
        failure(&db, "SELECT to_years(2147483647)"),
        "Interval value 2147483647 years out of range"
    );
}

/// The three spellings of a null check, end to end. Per #306.
///
/// Every answer and every column name below was read off the pinned binary. `nullif` is a macro
/// there, `CASE WHEN a = b THEN NULL ELSE a END`, and the two things that follow from that are worth
/// pointing at: a null on the right is not a match, because `1 = NULL` is null rather than true, and
/// the answer keeps the first argument's type even when the comparison had to widen to happen at all.
#[test]
fn the_null_checks_answer_the_way_duckdb_does() {
    let db = database();
    assert_eq!(rows(&db, "SELECT COALESCE(NULL, 1)"), vec![vec![Value::Integer(1)]]);
    assert_eq!(rows(&db, "SELECT coalesce(NULL, NULL, 3, 4)"), vec![vec![Value::Integer(3)]]);
    assert_eq!(rows(&db, "SELECT coalesce(NULL, NULL)"), vec![vec![Value::Null]]);
    assert_eq!(rows(&db, "SELECT coalesce(2)"), vec![vec![Value::Integer(2)]]);
    assert_eq!(
        rows(&db, "SELECT ifnull(NULL, 3), ifnull(1, 3)"),
        vec![vec![Value::Integer(3), Value::Integer(1)]]
    );
    assert_eq!(
        rows(&db, "SELECT nullif(1, 2), nullif(2, 2)"),
        vec![vec![Value::Integer(1), Value::Null]]
    );
    // A null on either side. The right one is not a match and the left one is the answer.
    assert_eq!(
        rows(&db, "SELECT nullif(1, NULL), nullif(NULL, 1)"),
        vec![vec![Value::Integer(1), Value::Null]]
    );
    assert_eq!(
        rows(&db, "SELECT nullif('a', 'a'), nullif('a', 'b')"),
        vec![vec![Value::Null, text("a")]]
    );
    assert_eq!(rows(&db, "SELECT nullif(TRUE, FALSE)"), vec![vec![Value::Boolean(true)]]);
    // The comparison happens at DECIMAL(11,1) and the answer is still an INTEGER, both measured.
    assert_eq!(
        rows(&db, "SELECT nullif(2, 2.5), typeof(nullif(2, 2.5))"),
        vec![vec![Value::Integer(2), text("INTEGER")]]
    );
    assert_eq!(
        rows(&db, "SELECT typeof(coalesce(1, 2.5)), typeof(nullif(1::BIGINT, 2::SMALLINT))"),
        vec![vec![text("DECIMAL(11,1)"), text("BIGINT")]]
    );
    // Over a column, where the null row is the one that moves.
    assert_eq!(
        rows(&db, "SELECT coalesce(s, 'none') FROM t"),
        vec![vec![text("a")], vec![text("none")], vec![text("c")], vec![text("a")]]
    );
    assert_eq!(
        rows(&db, "SELECT nullif(x, 1) FROM t"),
        vec![
            vec![Value::Integer(3)],
            vec![Value::Null],
            vec![Value::Integer(2)],
            vec![Value::Null]
        ]
    );
    // The names upstream gives these columns. `COALESCE` is an operator there rather than a
    // function, so it is printed in capitals whichever case was written and `IFNULL` becomes it.
    assert_eq!(
        db.query("SELECT coalesce(x, 1) FROM t").unwrap().names(),
        &["COALESCE(x, 1)".to_string()]
    );
    assert_eq!(
        db.query("SELECT IFNULL(x, 1) FROM t").unwrap().names(),
        &["COALESCE(x, 1)".to_string()]
    );
    // This one is quoted because NULLIF is a keyword, and `COALESCE` above it is not because that
    // one is an operator upstream rather than a name the deparser ever writes. Per #251.
    assert_eq!(
        db.query("SELECT NULLIF(x, 1) FROM t").unwrap().names(),
        &["\"nullif\"(x, 1)".to_string()]
    );
    // The counts the grammar refuses, and the one upstream's parser refuses itself.
    assert!(failure(&db, "SELECT nullif(1)").contains("syntax error at or near \")\""));
    assert!(failure(&db, "SELECT nullif(1, 2, 3)").contains("syntax error at or near \",\""));
    assert!(failure(&db, "SELECT coalesce()").contains("syntax error at or near \")\""));
    assert_eq!(failure(&db, "SELECT ifnull(1)"), "Wrong number of arguments to IFNULL.");
    assert_eq!(failure(&db, "SELECT ifnull(1, 2, 3)"), "Wrong number of arguments to IFNULL.");
}

/// `typeof` names the type of an expression, which is decided before a row moves. Per #229.
///
/// Every answer here was read off the pinned binary. The one worth pointing at is `typeof(NULL)`,
/// which is six characters and not four: the quotes are part of the name upstream prints and the
/// reason is visible in `typeof([])`, which comes back as `"NULL"[]`.
#[test]
fn typeof_answers_the_name_of_the_type() {
    let db = database();
    let named = |sql: &str| match rows(&db, sql).as_slice() {
        [row] => row.clone(),
        other => panic!("one row, not {}", other.len()),
    };
    assert_eq!(
        named("SELECT typeof(1), typeof(1.5), typeof('a'), typeof(TRUE), typeof(NULL)"),
        vec![
            text("INTEGER"),
            text("DECIMAL(2,1)"),
            text("VARCHAR"),
            text("BOOLEAN"),
            text("\"NULL\"")
        ]
    );
    assert_eq!(
        named("SELECT typeof(1::BIGINT), typeof('2024-01-01'::DATE), typeof(1 + 2), typeof(1 / 2)"),
        vec![text("BIGINT"), text("DATE"), text("INTEGER"), text("DOUBLE")]
    );
    // Over a table, where the type is the column's and the rows are all the same.
    assert_eq!(
        named("SELECT DISTINCT typeof(x), typeof(s), typeof(x + 1) FROM t"),
        vec![text("INTEGER"), text("VARCHAR"), text("INTEGER")]
    );
    // And over an aggregate, where the type is the one the aggregate accumulates in.
    assert_eq!(
        named("SELECT typeof(count(*)), typeof(sum(x)), typeof(avg(x)) FROM t"),
        vec![text("BIGINT"), text("HUGEINT"), text("DOUBLE")]
    );
    // The name a column gets is the call as it was written, which is what upstream calls it too.
    assert_eq!(db.query("SELECT typeof(x) FROM t").unwrap().names(), &["typeof(x)".to_string()]);
    // One argument, and the sentence for the other counts is the table's own.
    assert!(failure(&db, "SELECT typeof(1, 2)").contains("typeof(col0 ANY) -> VARCHAR"));
}

/// Brackets on a string, which the transformer writes as `array_extract` and `array_slice` and the
/// binder then resolves like any other call. Per #278.
#[test]
fn a_bracket_on_a_string_indexes_it_by_character() {
    let db = database();
    assert_eq!(rows(&db, "SELECT 'abcdef'[2]"), vec![vec![text("b")]]);
    assert_eq!(rows(&db, "SELECT 'abcdef'[-1]"), vec![vec![text("f")]]);
    // Off either end is the empty string rather than a null, which is a string only rule.
    assert_eq!(rows(&db, "SELECT 'abcdef'[0]"), vec![vec![text("")]]);
    assert_eq!(rows(&db, "SELECT 'abcdef'[9]"), vec![vec![text("")]]);
    assert_eq!(rows(&db, "SELECT 'héllo'[2]"), vec![vec![text("é")]]);
    assert_eq!(rows(&db, "SELECT 'abcdef'[2:4]"), vec![vec![text("bcd")]]);
    // The four ways of leaving a bound out, which the transformer fills in before the binder sees
    // them, and the two that clamp.
    assert_eq!(rows(&db, "SELECT 'abcdef'[:3]"), vec![vec![text("abc")]]);
    assert_eq!(rows(&db, "SELECT 'abcdef'[4:]"), vec![vec![text("def")]]);
    assert_eq!(rows(&db, "SELECT 'abcdef'[:]"), vec![vec![text("abcdef")]]);
    assert_eq!(rows(&db, "SELECT 'abcdef'[1:-]"), vec![vec![text("abcdef")]]);
    assert_eq!(rows(&db, "SELECT 'abcdef'[3:99]"), vec![vec![text("cdef")]]);
    assert_eq!(rows(&db, "SELECT 'abcdef'[4:2]"), vec![vec![text("")]]);
    // A column rather than a literal, and a null row, which stays null through both calls.
    assert_eq!(
        rows(&db, "SELECT s[1], s[1:1] FROM t"),
        vec![
            vec![text("a"), text("a")],
            vec![Value::Null, Value::Null],
            vec![text("c"), text("c")],
            vec![text("a"), text("a")],
        ]
    );
    // The same two calls written out, including the three spellings that are aliases of them.
    assert_eq!(rows(&db, "SELECT array_extract('abcdef', 2)"), vec![vec![text("b")]]);
    assert_eq!(rows(&db, "SELECT list_extract('abcdef', 2)"), vec![vec![text("b")]]);
    assert_eq!(rows(&db, "SELECT list_element('abcdef', 2)"), vec![vec![text("b")]]);
    assert_eq!(rows(&db, "SELECT list_slice('abcdef', 2, 4)"), vec![vec![text("bcd")]]);
}

/// One index into a list. Per #278.
#[test]
fn a_bracket_on_a_list_picks_one_element_out_of_it() {
    let db = database();
    assert_eq!(rows(&db, "SELECT [1,2,3][2]"), vec![vec![integer(2)]]);
    assert_eq!(rows(&db, "SELECT [1,2,3][-1]"), vec![vec![integer(3)]]);
    // Off either end of a list is a null, which is where the list and the string disagree.
    assert_eq!(rows(&db, "SELECT [1,2,3][0]"), vec![vec![Value::Null]]);
    assert_eq!(rows(&db, "SELECT [1,2,3][4]"), vec![vec![Value::Null]]);
    assert_eq!(rows(&db, "SELECT list_extract([1,2,3], 2)"), vec![vec![integer(2)]]);
}

/// The list half of #278, which could not be reached through SQL until #302.
///
/// Every rule here was measured in `rudb-kernels/src/subscript.rs` when the slice was written, and
/// none of it could be checked end to end because the answer is a list and a list could not be put in
/// a vector. So the rules were right and the path from the parser to the printed row was untested,
/// which is the split this project tries not to have. These are the same thirteen cases read off the
/// pinned binary.
#[test]
fn a_slice_of_a_list_answers_a_list() {
    let db = database();
    assert_eq!(rows(&db, "SELECT [1,2,3][1:2]"), vec![vec![list(&[1, 2])]]);
    assert_eq!(rows(&db, "SELECT [1,2,3][2:]"), vec![vec![list(&[2, 3])]]);
    assert_eq!(rows(&db, "SELECT [1,2,3][:2]"), vec![vec![list(&[1, 2])]]);
    assert_eq!(rows(&db, "SELECT [1,2,3][:]"), vec![vec![list(&[1, 2, 3])]]);
    // A list is one based, so zero is the same place one is rather than an error or an empty list.
    assert_eq!(rows(&db, "SELECT [1,2,3][0:2]"), vec![vec![list(&[1, 2])]]);
    assert_eq!(rows(&db, "SELECT [1,2,3][-2:-1]"), vec![vec![list(&[2, 3])]]);
    // Both ends are clamped rather than refused, so a slice can name rows that are not there and a
    // backwards slice is the empty list and not an error.
    assert_eq!(rows(&db, "SELECT [1,2,3][2:99]"), vec![vec![list(&[2, 3])]]);
    assert_eq!(rows(&db, "SELECT [1,2,3][-99:99]"), vec![vec![list(&[1, 2, 3])]]);
    assert_eq!(rows(&db, "SELECT [1,2,3][3:1]"), vec![vec![list(&[])]]);
    assert_eq!(rows(&db, "SELECT array_slice([1,2,3], 2, 3)"), vec![vec![list(&[2, 3])]]);
    assert_eq!(rows(&db, "SELECT list_slice([1,2,3], 1, 1)"), vec![vec![list(&[1])]]);
    // A null bound makes the whole slice null, and a null list slices to a null rather than to an
    // empty list, which is the difference #302 made the vector able to carry.
    assert_eq!(rows(&db, "SELECT [1,2,3][NULL:2]"), vec![vec![Value::Null]]);
    assert_eq!(rows(&db, "SELECT (NULL::INT[])[1:2]"), vec![vec![Value::Null]]);
}

/// A list column, stored and read back. Per #302.
#[test]
fn a_list_column_keeps_an_empty_list_and_a_null_apart() {
    let db = database();
    db.execute("CREATE TABLE lists (a INTEGER[])").unwrap();
    db.execute("INSERT INTO lists VALUES ([1,2,3]), (NULL), ([]), ([4])").unwrap();
    assert_eq!(
        rows(&db, "SELECT a FROM lists"),
        vec![vec![list(&[1, 2, 3])], vec![Value::Null], vec![list(&[])], vec![list(&[4])],]
    );
    // The mask is what tells the empty list from the null, so the count and the null test are the
    // two questions that would give the same answer if it did not.
    assert_eq!(rows(&db, "SELECT count(a) FROM lists"), vec![vec![Value::BigInt(3)]]);
    assert_eq!(
        rows(&db, "SELECT a IS NULL FROM lists"),
        vec![
            vec![Value::Boolean(false)],
            vec![Value::Boolean(true)],
            vec![Value::Boolean(false)],
            vec![Value::Boolean(false)],
        ]
    );
}

/// What the struct vector changes that a query can see today. Per #594.
///
/// One line, and that is the honest size of it. A struct vector exists now, so a query that has to put
/// a struct in one stops erroring, and the only such query a user can write is a cast of a null, because
/// the parser does not build a struct literal yet and there is no `struct_pack` to call instead. That is
/// the same split #302 left for lists and the same order: the vector first so the rest has somewhere to
/// compute into. Both answers here are the pin's.
#[test]
fn a_null_struct_is_a_value_now_rather_than_an_unwritten_vector() {
    let db = database();
    assert_eq!(rows(&db, "SELECT NULL::STRUCT(a INT)"), vec![vec![Value::Null]]);
    assert_eq!(
        rows(&db, "SELECT typeof(NULL::STRUCT(a INT, b VARCHAR))"),
        vec![vec![Value::Varchar("STRUCT(a INTEGER, b VARCHAR)".to_string())]]
    );
    // A struct column in a table is a column the catalog already held and the reader could not read.
    // It has no rows in it, because putting one in needs a literal the parser does not build.
    db.execute("CREATE TABLE structs (a STRUCT(x INTEGER, y VARCHAR))").unwrap();
    assert_eq!(rows(&db, "SELECT count(*) FROM structs"), vec![vec![Value::BigInt(0)]]);
    assert!(rows(&db, "SELECT a FROM structs").is_empty());
}

/// The same one line for the map vector. Per #595.
///
/// A map needs a `map()` function or a `MAP {}` literal before a query can build one, and it has
/// neither yet, so a cast of a null is again the whole visible surface. What it is really for is the
/// eight catalog columns in D2 that are typed `MAP(VARCHAR, VARCHAR)` and are the empty map in every
/// row, which could not be written at all while the form did not exist. Both answers here are the
/// pin's.
#[test]
fn a_null_map_is_a_value_now_rather_than_an_unwritten_vector() {
    let db = database();
    assert_eq!(rows(&db, "SELECT NULL::MAP(VARCHAR, VARCHAR)"), vec![vec![Value::Null]]);
    assert_eq!(
        rows(&db, "SELECT typeof(NULL::MAP(INT, INT))"),
        vec![vec![Value::Varchar("MAP(INTEGER, INTEGER)".to_string())]]
    );
    db.execute("CREATE TABLE maps (tags MAP(VARCHAR, VARCHAR))").unwrap();
    assert_eq!(rows(&db, "SELECT count(*) FROM maps"), vec![vec![Value::BigInt(0)]]);
    assert!(rows(&db, "SELECT tags FROM maps").is_empty());
}

/// `duckdb_types()` through the binder and the executor rather than through a hand written plan.
#[test]
fn the_types_table_answers_a_query_a_client_would_actually_write() {
    let db = database();
    assert_eq!(rows(&db, "SELECT count(*) FROM duckdb_types()"), vec![vec![Value::BigInt(93)]]);
    // A client reading this table is asking whether the engine has a type, so the useful query is a
    // name lookup, and it has to work through the where clause rather than only over the whole
    // table.
    assert_eq!(
        rows(&db, "SELECT logical_type, type_size FROM duckdb_types() WHERE type_name = 'hugeint'"),
        vec![vec![Value::Varchar("HUGEINT".to_string()), Value::BigInt(16)]]
    );
    // The name is case insensitive the way every function name is, and the table is in the default
    // catalog and schema because that is where the pin puts the builtin types.
    assert_eq!(
        rows(&db, "SELECT DISTINCT database_name, schema_name FROM DuckDB_Types()"),
        vec![vec![Value::Varchar("memory".to_string()), Value::Varchar("main".to_string())]]
    );
    // `tags` is why this table needed a map vector, so it is read back here as one rather than only
    // counted.
    assert_eq!(
        rows(&db, "SELECT tags FROM duckdb_types() WHERE type_name = 'boolean'"),
        vec![vec![Value::map(LogicalType::Varchar, LogicalType::Varchar, Vec::new())]]
    );
}

/// `duckdb_functions()` through the binder and the executor, per #465.
#[test]
fn the_functions_table_answers_the_question_a_client_asks_it() {
    let db = database();
    // The query a client actually writes, which is whether a name is there at all.
    assert_eq!(
        rows(&db, "SELECT count(*) FROM duckdb_functions() WHERE function_name = 'sqrt'"),
        vec![vec![Value::BigInt(0)]]
    );
    assert_eq!(
        rows(&db, "SELECT function_type FROM duckdb_functions() WHERE function_name = 'avg'"),
        vec![vec![Value::Varchar("aggregate".to_string())]]
    );
    // Builtins are reported in `system.main` and not in `memory`, which is where the pin puts them
    // and where every client query looks. rudb has no catalog named `system` yet, so this is the one
    // column in the table that names something the catalog does not have.
    assert_eq!(
        rows(&db, "SELECT DISTINCT database_name, schema_name FROM duckdb_functions()"),
        vec![vec![Value::Varchar("system".to_string()), Value::Varchar("main".to_string())]]
    );
    // Every row is a builtin, which is what makes the corpus query for user defined functions come
    // back empty rather than wrong.
    assert!(
        rows(&db, "SELECT function_name FROM duckdb_functions() WHERE NOT internal").is_empty()
    );
    // This table lists itself, because it is a table function and the table lists those.
    assert_eq!(
        rows(&db, "SELECT count(*) FROM duckdb_functions() WHERE function_name LIKE 'duckdb_%'"),
        vec![vec![Value::BigInt(13)]]
    );
}

/// The fourteen names that answer about the connection rather than about the query.
#[test]
fn the_session_context_answers_for_the_clock_the_catalog_and_the_user() {
    let db = database();
    db.execute("SET TimeZone = 'UTC'").expect("a stable zone for written timestamp answers");
    let text = |value: &str| Value::Varchar(value.to_string());
    // The types first, because the type is the half a client reads through a driver and the half
    // that is easy to get wrong. Every one of these was measured against the pin.
    assert_eq!(
        rows(
            &db,
            "SELECT typeof(now()), typeof(current_timestamp), typeof(get_current_timestamp()), \
             typeof(transaction_timestamp()), typeof(current_localtimestamp()), \
             typeof(localtimestamp)"
        ),
        vec![vec![
            text("TIMESTAMP WITH TIME ZONE"),
            text("TIMESTAMP WITH TIME ZONE"),
            text("TIMESTAMP WITH TIME ZONE"),
            text("TIMESTAMP WITH TIME ZONE"),
            text("TIMESTAMP"),
            text("TIMESTAMP"),
        ]]
    );
    assert_eq!(
        rows(
            &db,
            "SELECT typeof(current_date), typeof(today()), typeof(current_time), \
             typeof(get_current_time()), typeof(localtime), typeof(current_localtime())"
        ),
        vec![vec![
            text("DATE"),
            text("DATE"),
            text("TIME WITH TIME ZONE"),
            text("TIME WITH TIME ZONE"),
            text("TIME"),
            text("TIME"),
        ]]
    );
    // The four that name something the catalog knows, plus the user, which rudb answers the pin's
    // way because neither engine has users and a client asking wants a name.
    assert_eq!(
        rows(
            &db,
            "SELECT current_schema, current_schema(), current_catalog, current_database(), \
             current_user, session_user, user"
        ),
        vec![vec![
            text("main"),
            text("main"),
            text("memory"),
            text("memory"),
            text("duckdb"),
            text("duckdb"),
            text("duckdb"),
        ]]
    );
    // One instant per statement, whatever spells it and however many rows read it. That is what the
    // pin reports as CONSISTENT_WITHIN_QUERY and what it answers for the same query.
    assert_eq!(
        rows(
            &db,
            "SELECT now() = current_timestamp, now() = transaction_timestamp(), \
             now() = get_current_timestamp(), today() = current_date"
        ),
        vec![vec![
            Value::Boolean(true),
            Value::Boolean(true),
            Value::Boolean(true),
            Value::Boolean(true),
        ]]
    );
    assert_eq!(
        rows(&db, "SELECT count(DISTINCT n) FROM (SELECT now() AS n FROM range(3))"),
        vec![vec![Value::BigInt(1)]]
    );
    // The clock reads forward, which is the only thing about its value a test can hold it to.
    assert_eq!(
        rows(&db, "SELECT now() > TIMESTAMP '2024-01-01', current_date > DATE '2024-01-01'"),
        vec![vec![Value::Boolean(true), Value::Boolean(true)]]
    );
    // A column of the same name wins over the keyword, which was measured on the pin for all ten of
    // the bare spellings and is why the scope is asked before the fold.
    db.execute(
        "CREATE TABLE context(current_date VARCHAR, current_user VARCHAR, \"user\" VARCHAR)",
    )
    .expect("three columns named after keywords");
    db.execute("INSERT INTO context VALUES ('a', 'b', 'c')").expect("one row");
    assert_eq!(
        rows(&db, "SELECT current_date, current_user, user FROM context"),
        vec![vec![text("a"), text("b"), text("c")]]
    );
    // And a name two tables both carry is still the ambiguity error rather than the constant.
    assert_eq!(
        failure(&db, "SELECT current_date FROM context, context AS again"),
        "Ambiguous reference to column name \"current_date\" (use: 'context.current_date' or \
         'again.current_date')"
    );
    // The five names that take one spelling and not the other. `current_database` is a function and
    // not a keyword on the pin, and `current_timestamp` is a keyword and not a function.
    assert_eq!(
        failure(&db, "SELECT current_database"),
        "Referenced column \"current_database\" not found in FROM clause!"
    );
    assert_eq!(
        failure(&db, "SELECT current_timestamp()"),
        "Scalar Function with name current_timestamp does not exist!"
    );
    // A call with arguments is not one of these and reaches the signature table, which has a row per
    // name so that the message is the arity error the pin gives rather than a missing function.
    assert!(
        failure(&db, "SELECT now(1)")
            .starts_with("No function matches the given name and argument types 'now(INTEGER)'"),
        "an arity error rather than a missing function"
    );
    // A zoned value keeps its zone through arithmetic, which is the half of this that is not about
    // the clock at all: `now()` is the first way a query gets one of these, so every operator that
    // takes a timestamp had to learn the zoned kind. All of these were measured against the pin.
    assert_eq!(
        rows(
            &db,
            "SELECT typeof(now() + INTERVAL 1 DAY), typeof(INTERVAL 1 DAY + now()), \
             typeof(now() - now()), typeof(now() - TIMESTAMP '2020-01-01'), \
             typeof(now() - DATE '2020-01-01'), typeof(now() + NULL), \
             typeof(current_time + INTERVAL 1 HOUR), typeof(current_date + current_time)"
        ),
        vec![vec![
            text("TIMESTAMP WITH TIME ZONE"),
            text("TIMESTAMP WITH TIME ZONE"),
            text("INTERVAL"),
            text("INTERVAL"),
            text("INTERVAL"),
            text("TIMESTAMP WITH TIME ZONE"),
            text("TIME WITH TIME ZONE"),
            text("TIMESTAMP WITH TIME ZONE"),
        ]]
    );
    assert_eq!(
        rows(
            &db,
            "SELECT typeof(date_part('year', now())), typeof(date_trunc('day', now())), \
             typeof(age(now(), now())), \
             CAST(date_trunc('day', TIMESTAMPTZ '2020-01-02 03:04:05') AS VARCHAR), \
             CAST(TIMESTAMPTZ '2020-01-31 10:00:00' + INTERVAL 1 MONTH AS VARCHAR), \
             CAST(DATE '2020-01-02' + TIMETZ '03:04:05' AS VARCHAR)"
        ),
        vec![vec![
            text("BIGINT"),
            text("TIMESTAMP WITH TIME ZONE"),
            text("INTERVAL"),
            text("2020-01-02 00:00:00+00"),
            text("2020-02-29 10:00:00+00"),
            text("2020-01-02 03:04:05+00"),
        ]]
    );
    // All fourteen are in the function table, which is where a client looks to find out. The count
    // is of the distinct names rather than of the rows, because the pin has two rows for
    // `current_schema` and two for `current_database`, a scalar and a macro with the same name, and
    // the thing worth holding still is that the name is there rather than how many ways it is there.
    assert_eq!(
        rows(
            &db,
            "SELECT count(DISTINCT function_name) FROM duckdb_functions() WHERE function_name IN \
             ('now', 'today', \
             'get_current_timestamp', 'get_current_time', 'transaction_timestamp', \
             'current_localtime', 'current_localtimestamp', 'current_date', 'current_schema', \
             'current_database', 'current_catalog', 'current_user', 'session_user', 'user')"
        ),
        vec![vec![Value::BigInt(14)]]
    );
}

#[test]
fn the_session_time_zone_moves_local_context_and_one_argument_age() {
    let db = database();
    let default_zone = db.setting("TimeZone").expect("the operating-system zone");
    db.execute("SET TimeZone = 'America/New_York'").expect("an IANA zone");
    assert_eq!(
        rows(&db, "SELECT current_setting('timezone')"),
        vec![vec![Value::Varchar("America/New_York".to_string())]]
    );
    let answer = rows(&db, "SELECT now(), localtimestamp, current_time, localtime");
    let [
        Value::TimestampTz(utc),
        Value::Timestamp(local),
        Value::TimeTz(zoned_time),
        Value::Time(local_time),
    ] = &answer[0][..]
    else {
        panic!("the four context values had the wrong types: {:?}", answer[0]);
    };
    assert_eq!(local - utc, -4 * 60 * 60 * 1_000_000, "New York is EDT in September");
    assert_eq!(zoned_time, local_time);
    assert_eq!(
        rows(
            &db,
            "SELECT CAST(stamp AS VARCHAR) FROM (VALUES \
             (TIMESTAMPTZ '2020-01-01 12:00:00+00'), \
             (TIMESTAMPTZ '2020-07-01 12:00:00+00')) AS t(stamp)"
        ),
        vec![
            vec![Value::Varchar("2020-01-01 07:00:00-05".to_string())],
            vec![Value::Varchar("2020-07-01 08:00:00-04".to_string())],
        ]
    );
    assert_eq!(
        rows(&db, "VALUES (CAST(TIMESTAMPTZ '2020-07-01 12:00:00+00' AS VARCHAR))"),
        vec![vec![Value::Varchar("2020-07-01 08:00:00-04".to_string())]]
    );
    assert_eq!(
        rows(
            &db,
            "SELECT age(DATE '2020-02-28') = age(current_date, DATE '2020-02-28'), \
             age(TIMESTAMP '2020-02-28 12:00:00') = \
             age(current_date, TIMESTAMP '2020-02-28 12:00:00')"
        ),
        vec![vec![Value::Boolean(true), Value::Boolean(true)]]
    );
    db.execute("SET TIME ZONE 'Asia/Kathmandu'").expect("the standard spelling");
    assert_eq!(db.setting("TimeZone").expect("the zone"), "Asia/Kathmandu");
    db.execute("RESET TimeZone").expect("the default zone");
    assert_eq!(db.setting("timezone").expect("case is ignored"), default_zone);
    db.execute("SET TimeZone = 'UTC'").expect("a different zone");
    db.execute("SET TIME ZONE LOCAL").expect("the local zone");
    assert_eq!(db.setting("TimeZone").expect("the local zone"), default_zone);
    let error = db.execute("SET TimeZone = 'not/a_zone'").expect_err("an unknown zone");
    assert_eq!(error.code().duckdb_name(), "Not implemented Error");
    assert!(error.message().starts_with("Unknown TimeZone 'not/a_zone'!"), "{error}");
}

#[test]
fn the_session_sort_defaults_are_resolved_into_each_sort_key() {
    let db = database();
    db.execute("CREATE TABLE sort_defaults(x INTEGER)").expect("a nullable column");
    db.execute("INSERT INTO sort_defaults VALUES (1), (NULL), (2)").expect("three rows");
    let one = |value| vec![Value::Integer(value)];
    let null = vec![Value::Null];
    assert_eq!(
        rows(&db, "SELECT x FROM sort_defaults ORDER BY x DESC"),
        vec![one(2), one(1), null.clone()]
    );
    db.execute("SET default_order = 'descending'").expect("the descending default");
    assert_eq!(db.setting("default_order").expect("the direction"), "DESC");
    assert_eq!(
        rows(&db, "SELECT x FROM sort_defaults ORDER BY x"),
        vec![one(2), one(1), null.clone()]
    );
    db.execute("SET default_null_order = 'first'").expect("nulls first");
    assert_eq!(
        rows(&db, "SELECT x FROM sort_defaults ORDER BY x"),
        vec![null.clone(), one(2), one(1)]
    );
    db.execute("SET default_null_order = 'sqlite'").expect("the SQLite convention");
    assert_eq!(
        rows(&db, "SELECT x FROM sort_defaults ORDER BY x ASC"),
        vec![null.clone(), one(1), one(2)]
    );
    assert_eq!(
        rows(&db, "SELECT x FROM sort_defaults ORDER BY x DESC"),
        vec![one(2), one(1), null.clone()]
    );
    db.execute("SET default_null_order = 'postgres'").expect("the PostgreSQL convention");
    assert_eq!(
        rows(&db, "SELECT x FROM sort_defaults ORDER BY x DESC"),
        vec![null, one(2), one(1)]
    );
    db.execute("RESET default_order").expect("the direction default");
    db.execute("RESET default_null_order").expect("the null default");
    assert_eq!(db.setting("default_order").expect("the direction"), "ASCENDING");
    assert_eq!(db.setting("default_null_order").expect("the null order"), "NULLS_LAST");
}

#[test]
fn integer_division_is_resolved_while_the_expression_is_bound() {
    let db = database();
    assert_eq!(
        rows(&db, "SELECT 7 / 2, typeof(7 / 2)"),
        vec![vec![Value::Double(3.5), text("DOUBLE")]]
    );
    db.execute("SET integer_division = true").expect("integer division");
    assert_eq!(db.setting("integer_division").expect("the setting"), "true");
    assert_eq!(
        rows(&db, "SELECT 7 / 2, typeof(7 / 2), 7.5 / 2.0, typeof(7.5 / 2.0)"),
        vec![vec![Value::Integer(3), text("INTEGER"), Value::Double(3.75), text("DOUBLE")]]
    );
    assert_eq!(db.query("SELECT 7 / 2").expect("a division").names(), ["(7 // 2)".to_string()]);
    db.execute("RESET integer_division").expect("floating point division");
    assert_eq!(db.setting("integer_division").expect("the setting"), "false");
    assert_eq!(rows(&db, "SELECT 7 / 2"), vec![vec![Value::Double(3.5)]]);
    let error = db.execute("SET integer_division = 'off'").expect_err("not a boolean");
    assert_eq!(error.message(), "Failed to cast value: Could not convert string 'off' to BOOL");
}

#[test]
fn zero_division_nulls_are_resolved_while_the_expression_is_bound() {
    let db = database();
    assert_eq!(db.setting("null_on_division_by_zero").expect("the setting"), "false");
    assert!(db.query("SELECT 1 // 0").is_err());
    db.execute("SET null_on_division_by_zero = true").expect("nulling division errors");
    assert_eq!(db.setting("null_on_division_by_zero").expect("the setting"), "true");
    assert_eq!(
        rows(
            &db,
            "SELECT current_setting('null_on_division_by_zero'), typeof(current_setting('null_on_division_by_zero'))"
        ),
        vec![vec![Value::Boolean(true), text("BOOLEAN")]]
    );
    assert_eq!(
        rows(
            &db,
            "SELECT 1 / 0, 1 // 0, 1 % 0, 1.0 // 0.0, 1.0 % 0.0, CAST(1 AS FLOAT) / CAST(0 AS FLOAT)"
        ),
        vec![vec![
            Value::Double(f64::INFINITY),
            Value::Null,
            Value::Null,
            Value::Null,
            Value::Null,
            Value::Float(f32::INFINITY),
        ]]
    );
    assert_eq!(
        rows(&db, "SELECT a, 10 // a, 10 % a FROM range(-1, 2) t(a)"),
        vec![
            vec![Value::BigInt(-1), Value::BigInt(-10), Value::BigInt(0)],
            vec![Value::BigInt(0), Value::Null, Value::Null],
            vec![Value::BigInt(1), Value::BigInt(10), Value::BigInt(0)],
        ]
    );
    db.execute("RESET null_on_division_by_zero").expect("division errors");
    assert_eq!(db.setting("null_on_division_by_zero").expect("the setting"), "false");
    assert!(db.query("SELECT 1 // 0").is_err());
    let error = db.execute("SET null_on_division_by_zero = 'off'").expect_err("not a boolean");
    assert_eq!(error.message(), "Failed to cast value: Could not convert string 'off' to BOOL");
}

#[test]
fn ieee_floating_point_ops_are_resolved_while_the_expression_is_bound() {
    let db = database();
    assert_eq!(db.setting("ieee_floating_point_ops").expect("the setting"), "true");
    assert!(
        matches!(rows(&db, "SELECT 0.0::DOUBLE / 0.0::DOUBLE")[0][0], Value::Double(answer) if answer.is_nan())
    );
    assert!(
        matches!(rows(&db, "SELECT 1.0::DOUBLE % 0.0::DOUBLE")[0][0], Value::Double(answer) if answer.is_nan())
    );
    db.execute("SET ieee_floating_point_ops = false").expect("checked floating operations");
    assert_eq!(db.setting("ieee_floating_point_ops").expect("the setting"), "false");
    assert_eq!(
        rows(
            &db,
            "SELECT current_setting('ieee_floating_point_ops'), typeof(current_setting('ieee_floating_point_ops'))"
        ),
        vec![vec![Value::Boolean(false), text("BOOLEAN")]]
    );
    let advice = "Use TRY(...) to return NULL for this expression, or SET null_on_division_by_zero=true to return NULL for all divisions by zero.";
    assert_eq!(
        failure(&db, "SELECT 1.0::DOUBLE / 0.0::DOUBLE"),
        format!("Division by zero in expression (1.0 / 0.0). {advice}")
    );
    assert_eq!(
        failure(&db, "SELECT 1.0::DOUBLE % 0.0::DOUBLE"),
        format!("Division by zero in expression (1.0 % 0.0). {advice}")
    );
    db.create_table("ieee_values", vec![Field::new("a", LogicalType::Double)]).unwrap();
    db.append(
        "ieee_values",
        &[vec![Value::Double(-1.0)], vec![Value::Double(0.0)], vec![Value::Double(1.0)]],
    )
    .unwrap();
    assert_eq!(
        failure(&db, "SELECT 1.0 / a FROM ieee_values"),
        format!("Division by zero in expression (1.0 / a). {advice}")
    );
    assert_eq!(
        failure(&db, "SELECT 1.0 % a FROM ieee_values"),
        format!("Division by zero in expression (1.0 % a). {advice}")
    );
    db.execute("SET null_on_division_by_zero = true").expect("nulling division errors");
    assert_eq!(
        rows(&db, "SELECT 1.0::DOUBLE / 0.0::DOUBLE, 1.0::DOUBLE % 0.0::DOUBLE"),
        vec![vec![Value::Null, Value::Null]]
    );
    db.execute("RESET null_on_division_by_zero").expect("division errors");
    db.execute("RESET ieee_floating_point_ops").expect("IEEE floating operations");
    assert_eq!(db.setting("ieee_floating_point_ops").expect("the setting"), "true");
    let error = db.execute("SET ieee_floating_point_ops = 'off'").expect_err("not a boolean");
    assert_eq!(error.message(), "Failed to cast value: Could not convert string 'off' to BOOL");
}

#[test]
fn timestamp_to_timestamptz_casts_can_be_disabled_while_binding() {
    let db = database();
    db.execute("SET TimeZone = 'UTC'").expect("a deterministic zone");
    assert_eq!(db.setting("disable_timestamptz_casts").expect("the setting"), "false");
    assert_eq!(
        rows(&db, "SELECT TIMESTAMP '2024-01-02 03:04:05'::TIMESTAMPTZ"),
        vec![vec![Value::TimestampTz(1_704_164_645_000_000)]]
    );
    db.execute("SET disable_timestamptz_casts = true").expect("disabled timestamp casts");
    assert_eq!(
        rows(
            &db,
            "SELECT current_setting('disable_timestamptz_casts'), typeof(current_setting('disable_timestamptz_casts'))"
        ),
        vec![vec![Value::Boolean(true), text("BOOLEAN")]]
    );
    let expected = "Casting from TIMESTAMP to TIMESTAMP WITH TIME ZONE without an explicit time zone has been disabled  - use \"AT TIME ZONE ...\"";
    for query in [
        "SELECT TIMESTAMP '2024-01-02 03:04:05'::TIMESTAMPTZ",
        "SELECT TRY_CAST(TIMESTAMP '2024-01-02 03:04:05' AS TIMESTAMPTZ)",
        "SELECT TIMESTAMP '2024-01-02 03:04:05' = TIMESTAMPTZ '2024-01-02 03:04:05+00'",
        "SELECT CASE WHEN true THEN TIMESTAMP '2024-01-02 03:04:05' ELSE TIMESTAMPTZ '2024-01-02 03:04:05+00' END",
        "SELECT DATE '2024-01-02'::TIMESTAMPTZ",
        "VALUES (TIMESTAMP '2024-01-02 03:04:05'), (TIMESTAMPTZ '2024-01-02 03:04:05+00')",
        "SELECT TIMESTAMP '2024-01-02 03:04:05' UNION ALL SELECT TIMESTAMPTZ '2024-01-02 03:04:05+00'",
    ] {
        assert_eq!(failure(&db, query), expected, "{query}");
    }
    db.create_table("zoned", vec![Field::new("z", LogicalType::TimestampTz)]).unwrap();
    assert_eq!(failure(&db, "INSERT INTO zoned SELECT TIMESTAMP '2024-01-02 03:04:05'"), expected);
    assert_eq!(
        rows(&db, "SELECT '2024-01-02 03:04:05'::TIMESTAMPTZ"),
        vec![vec![Value::TimestampTz(1_704_164_645_000_000)]]
    );
    db.execute("RESET disable_timestamptz_casts").expect("timestamp casts restored");
    assert_eq!(db.setting("disable_timestamptz_casts").expect("the setting"), "false");
    let error = db.execute("SET disable_timestamptz_casts = 'off'").expect_err("not a boolean");
    assert_eq!(error.message(), "Failed to cast value: Could not convert string 'off' to BOOL");
    assert_eq!(
        rows(
            &db,
            "SELECT description, input_type, scope FROM duckdb_settings() WHERE name = 'disable_timestamptz_casts'"
        ),
        vec![vec![
            text("Disable casting from timestamp to timestamptz "),
            text("BOOLEAN"),
            text("GLOBAL"),
        ]]
    );
}

#[test]
fn a_non_integer_order_literal_needs_the_session_opt_in() {
    let db = database();
    let expected = "ORDER BY non-integer literal has no effect.\n* SET order_by_non_integer_literal=true to allow this behavior.";
    assert_eq!(failure(&db, "SELECT 2 AS x ORDER BY 'a'"), expected);
    assert_eq!(failure(&db, "SELECT 2 AS x ORDER BY 1.5"), expected);
    assert_eq!(failure(&db, "SELECT 2 AS x ORDER BY NULL"), expected);
    db.execute("SET order_by_non_integer_literal = true").expect("constant sort keys");
    assert_eq!(db.setting("order_by_non_integer_literal").expect("the setting"), "true");
    assert_eq!(rows(&db, "SELECT 2 AS x ORDER BY 'a'"), vec![vec![Value::Integer(2)]]);
    db.execute("RESET order_by_non_integer_literal").expect("the default guard");
    assert_eq!(db.setting("order_by_non_integer_literal").expect("the setting"), "false");
}

#[test]
fn regex_operator_semantics_are_resolved_while_the_expression_is_bound() {
    let db = database();
    assert_eq!(
        rows(&db, "SELECT 'abc' ~ 'b', 'abc' !~ 'b', 'ABC' ~* 'b', 'ABC' !~* 'b'"),
        vec![vec![
            Value::Boolean(true),
            Value::Boolean(false),
            Value::Boolean(true),
            Value::Boolean(false),
        ]]
    );
    db.execute("SET regex_match_operator_semantics = 'full'").expect("full matching");
    assert_eq!(
        rows(&db, "SELECT 'abc' ~ 'b', 'abc' !~ 'b', 'ABC' ~* 'b', 'ABC' !~* 'b'"),
        vec![vec![
            Value::Boolean(false),
            Value::Boolean(true),
            Value::Boolean(false),
            Value::Boolean(true),
        ]]
    );
    assert_eq!(
        db.query("SELECT 'abc' ~ 'b'").expect("a regex match").names(),
        ["regexp_full_match('abc', 'b')".to_string()]
    );
    assert_eq!(rows(&db, "SELECT 'abc' SIMILAR TO 'b'"), vec![vec![Value::Boolean(false)]]);
    db.execute("RESET regex_match_operator_semantics").expect("partial matching");
    assert_eq!(db.setting("regex_match_operator_semantics").expect("the setting"), "partial");
    let error =
        db.execute("SET regex_match_operator_semantics = 'nope'").expect_err("an unknown mode");
    assert_eq!(error.code().duckdb_name(), "Not implemented Error");
    assert_eq!(
        error.message(),
        "Enum value: unrecognized value \"nope\" for enum \"RegexMatchOperatorSemantics\"\n\nCandidates: \"FULL\""
    );
}

#[test]
fn show_behavior_resolves_a_name_before_execution() {
    let db = database();
    assert_eq!(rows(&db, "SHOW show_behavior"), vec![vec![text("AUTO")]]);
    let mixed = db.query("SHOW ShOw_BeHaViOr").expect("setting names ignore case");
    assert_eq!(mixed.names(), ["ShOw_BeHaViOr"]);
    assert_eq!(mixed.rows().collect::<Vec<_>>(), vec![vec![text("AUTO")]]);
    assert_eq!(
        rows(&db, "SHOW t"),
        vec![
            vec![text("x"), text("INTEGER"), text("YES"), Value::Null, Value::Null, Value::Null],
            vec![text("s"), text("VARCHAR"), text("YES"), Value::Null, Value::Null, Value::Null],
        ]
    );
    db.execute("SET show_behavior = 'setting'").expect("settings only");
    assert_eq!(rows(&db, "SHOW show_behavior"), vec![vec![text("setting")]]);
    assert_eq!(failure(&db, "SHOW t"), "Setting with name \"t\" does not exist");
    db.execute("SET show_behavior = 'table'").expect("tables only");
    assert_eq!(failure(&db, "SHOW show_behavior"), "Table with name show_behavior does not exist!");
    db.execute("RESET show_behavior").expect("automatic resolution");
    assert_eq!(db.setting("show_behavior").expect("the setting"), "AUTO");
    assert_eq!(
        rows(
            &db,
            "SELECT description, input_type, scope FROM duckdb_settings() WHERE name = 'show_behavior'"
        ),
        vec![vec![
            text(
                "How SHOW resolves a bare identifier: 'auto' (describe a table if one exists, else a setting; deprecated), 'table' (always a table), or 'setting' (always a setting)"
            ),
            text("VARCHAR"),
            text("GLOBAL"),
        ]]
    );
}

#[test]
fn current_dialect_resolves_through_the_installed_parser_registry() {
    let db = database();
    assert_eq!(rows(&db, "SELECT current_setting('current_dialect')"), vec![vec![text("duckdb")]]);
    db.execute("SET current_dialect = 'DUCKDB'").expect("the installed dialect");
    assert_eq!(db.setting("current_dialect").expect("the dialect"), "DUCKDB");
    assert_eq!(rows(&db, "SELECT 1"), vec![vec![Value::Integer(1)]]);
    let error = db.execute("SET current_dialect = 'cypher'").expect_err("not installed");
    assert_eq!(error.code().duckdb_name(), "Invalid Input Error");
    assert_eq!(error.message(), "Dialect \"cypher\" is not installed");
    db.execute("RESET current_dialect").expect("the default dialect");
    assert_eq!(db.setting("current_dialect").expect("the dialect"), "duckdb");
    assert_eq!(
        rows(
            &db,
            "SELECT description, input_type, scope FROM duckdb_settings() WHERE name = 'current_dialect'"
        ),
        vec![vec![text("The SQL dialect used by the parser"), text("VARCHAR"), text("GLOBAL")]]
    );
}

#[test]
fn dialect_compatibility_mode_accepts_exactly_the_modes_the_pin_has() {
    let db = database();
    assert_eq!(
        rows(&db, "SELECT current_setting('dialect_compatibility_mode')"),
        vec![vec![text("NONE")]]
    );
    db.execute("SET dialect_compatibility_mode = 'spark'").expect("Spark mode");
    assert_eq!(db.setting("dialect_compatibility_mode").expect("the mode"), "spark");
    db.execute("SET GLOBAL dialect_compatibility_mode = 'SPARK'").expect("global Spark mode");
    assert_eq!(db.setting("dialect_compatibility_mode").expect("the mode"), "SPARK");
    db.execute("SET dialect_compatibility_mode = 'none'").expect("no compatibility mode");
    assert_eq!(db.setting("dialect_compatibility_mode").expect("the mode"), "none");
    db.execute("RESET GLOBAL dialect_compatibility_mode").expect("the default mode");
    assert_eq!(db.setting("dialect_compatibility_mode").expect("the mode"), "NONE");
    let error = db.execute("SET dialect_compatibility_mode = 'nope'").expect_err("an unknown mode");
    assert_eq!(error.code().duckdb_name(), "Not implemented Error");
    assert_eq!(
        error.message(),
        "Enum value: unrecognized value \"nope\" for enum \"DialectCompatibilityMode\"\n\nCandidates: \"NONE\""
    );
    assert_eq!(
        rows(
            &db,
            "SELECT description, input_type, scope FROM duckdb_settings() WHERE name = 'dialect_compatibility_mode'"
        ),
        vec![vec![
            text(
                "Enable SQL dialect compatibility for a certain engine (e.g. `SET dialect_compatibility_mode='spark'`)"
            ),
            text("VARCHAR"),
            text("GLOBAL"),
        ]]
    );
}

#[test]
fn preserve_identifier_case_folds_only_unquoted_identifiers() {
    let db = database();
    assert_eq!(db.setting("preserve_identifier_case").expect("the mode"), "preserve_case");
    db.execute("CREATE TABLE MixedTable (MixedColumn INTEGER)").expect("preserved names");
    assert_eq!(
        rows(
            &db,
            "SELECT table_name, column_name FROM duckdb_columns() WHERE table_name = 'MixedTable'"
        ),
        vec![vec![text("MixedTable"), text("MixedColumn")]]
    );
    db.execute("SET preserve_identifier_case = 'lowercase'").expect("lowercase names");
    db.execute("CREATE TABLE LowerTable (LowerColumn INTEGER)").expect("lowercase table");
    assert_eq!(
        rows(
            &db,
            "SELECT table_name, column_name FROM duckdb_columns() WHERE table_name = 'lowertable'"
        ),
        vec![vec![text("lowertable"), text("lowercolumn")]]
    );
    db.execute("SET preserve_identifier_case = 'uppercase'").expect("uppercase names");
    db.execute("CREATE TABLE UpperTable (UpperColumn INTEGER)").expect("uppercase table");
    db.execute("CREATE TABLE \"QuotedTable\" (\"QuotedColumn\" INTEGER)").expect("quoted names");
    assert_eq!(
        rows(
            &db,
            "SELECT table_name, column_name FROM duckdb_columns() WHERE table_name IN ('UPPERTABLE', 'QuotedTable') ORDER BY table_name"
        ),
        vec![
            vec![text("QuotedTable"), text("QuotedColumn")],
            vec![text("UPPERTABLE"), text("UPPERCOLUMN")],
        ]
    );
    assert_eq!(rows(&db, "SELECT 'MixedString'"), vec![vec![text("MixedString")]]);
    db.execute("RESET preserve_identifier_case").expect("the default mode");
    assert_eq!(db.setting("preserve_identifier_case").expect("the mode"), "preserve_case");
}

#[test]
fn preserve_identifier_case_keeps_legacy_boolean_aliases_and_metadata() {
    let db = database();
    for truthy in ["true", "1", "'t'", "'y'", "'yes'"] {
        db.execute(&format!("SET preserve_identifier_case = {truthy}")).expect("truthy alias");
        assert_eq!(db.setting("preserve_identifier_case").expect("the mode"), "preserve_case");
    }
    for falsy in ["false", "0", "'f'", "'n'", "'no'"] {
        db.execute(&format!("SET preserve_identifier_case = {falsy}")).expect("falsy alias");
        assert_eq!(db.setting("preserve_identifier_case").expect("the mode"), "lowercase");
    }
    let invalid = db.execute("SET preserve_identifier_case = 'bogus'").expect_err("invalid mode");
    assert_eq!(invalid.code().duckdb_name(), "Invalid Input Error");
    assert_eq!(
        invalid.message(),
        "Unrecognized parameter for option preserve_identifier_case \"bogus\", expected one of: preserve_case, lowercase, uppercase"
    );
    let null = db.execute("SET preserve_identifier_case = NULL").expect_err("null mode");
    assert_eq!(null.message(), "preserve_identifier_case setting cannot be NULL");
    assert_eq!(
        rows(
            &db,
            "SELECT description, input_type, scope FROM duckdb_settings() WHERE name = 'preserve_identifier_case'"
        ),
        vec![vec![
            text(
                "How to fold non-quoted identifiers: 'preserve_case' keeps the case as written, 'lowercase' lowercases them, 'uppercase' uppercases them"
            ),
            text("VARCHAR"),
            text("GLOBAL"),
        ]]
    );
}

#[test]
fn allow_parser_override_extension_matches_the_only_installed_mode() {
    let db = database();
    assert_eq!(
        rows(&db, "SELECT current_setting('allow_parser_override_extension')"),
        vec![vec![text("DEFAULT")]]
    );
    db.execute("SET allow_parser_override_extension = DEFAULT").expect("the default mode");
    db.execute("SET allow_parser_override_extension = 'default'").expect("case insensitive mode");
    let invalid = db
        .execute("SET allow_parser_override_extension = true")
        .expect_err("the pin has no enabled mode");
    assert_eq!(invalid.code().duckdb_name(), "Not implemented Error");
    assert_eq!(
        invalid.message(),
        "Enum value: unrecognized value \"true\" for enum \"AllowParserOverride\"\n\nCandidates: \"DEFAULT\""
    );
    db.execute("RESET allow_parser_override_extension").expect("reset the mode");
    assert_eq!(db.setting("allow_parser_override_extension").expect("the mode"), "DEFAULT");
    assert_eq!(
        rows(
            &db,
            "SELECT description, input_type, scope FROM duckdb_settings() WHERE name = 'allow_parser_override_extension'"
        ),
        vec![vec![
            text("Allow extensions to override the current parser"),
            text("VARCHAR"),
            text("GLOBAL"),
        ]]
    );
}

#[test]
fn warnings_as_errors_matches_the_pin_without_a_logger() {
    let db = database();
    assert_eq!(
        rows(&db, "SELECT current_setting('warnings_as_errors')"),
        vec![vec![Value::Boolean(false)]]
    );
    db.execute("SET warnings_as_errors = false").expect("warnings stay warnings");
    db.execute("SET warnings_as_errors = 'no'").expect("the boolean alias");
    let error = db.execute("SET warnings_as_errors = true").expect_err("there is no logger");
    assert_eq!(error.code().duckdb_name(), "Settings Error");
    assert_eq!(
        error.message(),
        "Can not set 'warnings_as_errors=true'; no logger is available. To solve, run: 'SET enable_logging=true;'"
    );
    assert_eq!(db.setting("warnings_as_errors").expect("the setting"), "false");
    db.execute("RESET warnings_as_errors").expect("the default");
    assert_eq!(
        rows(
            &db,
            "SELECT description, input_type, scope FROM duckdb_settings() WHERE name = 'warnings_as_errors'"
        ),
        vec![vec![text("Escalate all warnings to errors."), text("BOOLEAN"), text("GLOBAL")]]
    );
}

#[test]
fn errors_as_json_structures_errors_at_every_sql_api_boundary() {
    let db = database();
    assert_eq!(
        rows(&db, "SELECT current_setting('errors_as_json')"),
        vec![vec![Value::Boolean(false)]]
    );
    db.execute("SET errors_as_json = true").expect("JSON errors");

    let missing = db.query("SELECT * FROM nonexistent_table").expect_err("missing table");
    assert_eq!(missing.code().duckdb_name(), "Catalog Error");
    assert!(missing.message().contains("\"exception_type\":\"Catalog\""));
    assert!(missing.message().contains("\"error_subtype\":\"MISSING_ENTRY\""));
    assert!(!missing.to_string().starts_with("Catalog Error:"));

    let column = db.query("SELECT cbl FROM (VALUES (42)) t(col)").expect_err("missing column");
    assert!(column.message().contains("\"exception_type\":\"Binder\""));
    assert!(column.message().contains("\"error_subtype\":\"COLUMN_NOT_FOUND\""));

    let syntax = db.prepare("SECT 1").expect_err("syntax error");
    assert!(syntax.message().contains("\"exception_type\":\"Parser\""));
    assert!(syntax.message().contains("\"error_subtype\":\"SYNTAX_ERROR\""));
    assert!(syntax.message().contains("\"position\":"));

    assert!(db.plan("SELECT missing").expect_err("plan error").message().starts_with('{'));
    db.execute("RESET errors_as_json").expect("plain errors");
    assert!(
        db.query("SELECT * FROM nonexistent_table")
            .expect_err("plain error")
            .to_string()
            .starts_with("Catalog Error:")
    );
    assert_eq!(
        rows(
            &db,
            "SELECT description, input_type, scope FROM duckdb_settings() WHERE name = 'errors_as_json'"
        ),
        vec![vec![
            text("Output error messages as structured JSON instead of as a raw string"),
            text("BOOLEAN"),
            text("GLOBAL"),
        ]]
    );
}

#[test]
fn the_settings_table_reads_back_what_set_left_behind() {
    let db = database();
    let text = |value: &str| Value::Varchar(value.to_string());
    // A value read through a database is a real one, unlike the same table built with no database
    // behind it, which is what `rudb_exec` tests against. What the default is depends on how much
    // memory the machine has, so what is checked is that something answered rather than what.
    assert!(
        rows(
            &db,
            "SELECT value FROM duckdb_settings() WHERE name = 'memory_limit' AND value IS NOT NULL"
        )
        .len()
            == 1
    );
    db.execute("SET memory_limit = '1GiB'").expect("a size");
    assert_eq!(
        rows(&db, "SELECT value, typed_value FROM duckdb_settings() WHERE name = 'memory_limit'"),
        vec![vec![text("1.0 GiB"), text("1.0 GiB")]]
    );
    // The alias is a row of its own and it reports the same value, because it is the same setting.
    assert_eq!(
        rows(&db, "SELECT value FROM duckdb_settings() WHERE name = 'max_memory'"),
        vec![vec![text("1.0 GiB")]]
    );
    // And writing through the alias moves the setting the other name reads.
    db.execute("SET max_memory = '2GiB'").expect("a size");
    assert_eq!(
        rows(&db, "SELECT value FROM duckdb_settings() WHERE name = 'memory_limit'"),
        vec![vec![text("2.0 GiB")]]
    );
    db.execute("SET worker_threads = 3").expect("a thread count");
    assert_eq!(
        rows(&db, "SELECT value FROM duckdb_settings() WHERE name = 'threads'"),
        vec![vec![text("3")]]
    );
}

#[test]
fn a_setting_read_as_a_value_has_the_type_the_setting_holds() {
    let db = database();
    db.execute("SET threads = 3").expect("a thread count");
    db.execute("SET memory_limit = '1GiB'").expect("a size");
    // A BIGINT and a VARCHAR out of one overload, which is the whole reason the declared return
    // type is ANY. Both were read off the pin.
    assert_eq!(rows(&db, "SELECT current_setting('threads')"), vec![vec![Value::BigInt(3)]]);
    assert_eq!(rows(&db, "SELECT typeof(current_setting('threads'))"), vec![vec![text("BIGINT")]]);
    assert_eq!(rows(&db, "SELECT current_setting('memory_limit')"), vec![vec![text("1.0 GiB")]]);
    assert_eq!(
        rows(&db, "SELECT typeof(current_setting('memory_limit'))"),
        vec![vec![text("VARCHAR")]]
    );
    // An alias reads the setting it points at, both ways round.
    assert_eq!(rows(&db, "SELECT current_setting('worker_threads')"), vec![vec![Value::BigInt(3)]]);
    assert_eq!(rows(&db, "SELECT current_setting('max_memory')"), vec![vec![text("1.0 GiB")]]);
    // The name is matched the way an identifier is, which the pin does as well.
    assert_eq!(rows(&db, "SELECT current_setting('THREADS')"), vec![vec![Value::BigInt(3)]]);
    // The column is named after what was written, since nothing renames a folded call.
    assert_eq!(
        db.query("SELECT current_setting('threads')").expect("a setting").names(),
        ["current_setting('threads')".to_string()]
    );
    // And it reads the setting as it is now rather than as it was when the database opened.
    db.execute("RESET threads").expect("a reset");
    let before = db.opened_with().threads();
    assert_eq!(
        rows(&db, "SELECT current_setting('threads')"),
        vec![vec![Value::BigInt(i64::try_from(before).expect("a thread count fits"))]]
    );
}

#[test]
fn a_setting_read_as_a_value_is_folded_before_the_plan_exists() {
    let db = database();
    db.execute("SET threads = 7").expect("a thread count");
    // The plan holds the number and not the call, which is what the pin does: an EXPLAIN there
    // shows `Projections: 6` over a dummy scan rather than a function over one.
    // The name survives as the column's alias, which is what the pin calls it too, so what is
    // asserted is that the thing projected is the number.
    let plan = db.plan("SELECT current_setting('threads')").expect("a plan");
    assert!(plan.contains("[7::BIGINT AS \"current_setting('threads')\"]"), "{plan}");
}

#[test]
fn a_setting_that_is_not_a_constant_or_not_a_setting_is_refused_the_pins_way() {
    let db = database();
    // A column of names cannot be folded, so the call falls through to the signature table and
    // gets the sentence the pin prints for exactly this.
    assert_eq!(
        failure(&db, "SELECT current_setting(s) FROM t"),
        "The \"setting_name\" argument in function \"current_setting\" must be a constant expression"
    );
    // A name nobody has is the same sentence `SET` gives for it, which is one sentence in one place.
    let unknown = failure(&db, "SELECT current_setting('nope')");
    assert!(unknown.starts_with("unrecognized configuration parameter \"nope\""), "{unknown}");
    assert!(unknown.contains("\"disabled_optimizers\""), "{unknown}");
    assert_eq!(unknown, db.execute("SET nope = 1").unwrap_err().message());
    // The wrong number of arguments is the ordinary arity error with the one overload under it.
    assert_eq!(
        failure(&db, "SELECT current_setting()"),
        "No function matches the given name and argument types 'current_setting()'. You might \
         need to add explicit type casts.\n\tCandidate functions:\n\tcurrent_setting(setting_name \
         VARCHAR) -> ANY\n"
    );
}

#[test]
fn the_settings_table_answers_the_question_a_client_asks_it() {
    let db = database();
    let text = |value: &str| Value::Varchar(value.to_string());
    // Twenty one rows for nineteen settings, because the pin gives an alias a row of its own.
    assert_eq!(rows(&db, "SELECT count(*) FROM duckdb_settings()"), vec![vec![Value::BigInt(21)]]);
    // The description is the pin's sentence word for word, since a client comparing them would
    // otherwise see a difference that is not one.
    assert_eq!(
        rows(
            &db,
            "SELECT description, input_type, scope FROM duckdb_settings() WHERE name = 'threads'"
        ),
        vec![vec![
            text("The number of total threads used by the system."),
            text("BIGINT"),
            text("GLOBAL"),
        ]]
    );
    // The seams are not settings, which is decided in the settings module and checked here because
    // this is the table a reader would find them in if the decision ever changed by accident.
    assert!(rows(&db, "SELECT name FROM duckdb_settings() WHERE name LIKE 'seam%'").is_empty());
}

#[test]
fn the_databases_and_schemas_tables_describe_the_catalogs_there_are() {
    let db = database();
    let text = |value: &str| Value::Varchar(value.to_string());
    let yes = Value::Boolean(true);
    let no = Value::Boolean(false);
    // Three databases and five schemas, which is what the pin returns from a session that has
    // attached nothing. `memory` is the one a person creates in and the other two are the engine's.
    assert_eq!(
        rows(
            &db,
            "SELECT database_name, internal, type, readonly FROM duckdb_databases() ORDER BY 1"
        ),
        vec![
            vec![text("memory"), no.clone(), text("duckdb"), no.clone()],
            vec![text("system"), yes.clone(), text("duckdb"), no.clone()],
            vec![text("temp"), yes.clone(), text("duckdb"), no.clone()],
        ]
    );
    // Internal on every row including `memory.main`, which is the pin's answer and reads oddly
    // until you notice that nobody made that schema either.
    assert_eq!(
        rows(
            &db,
            "SELECT database_name, schema_name, internal FROM duckdb_schemas() ORDER BY 1, 2"
        ),
        vec![
            vec![text("memory"), text("main"), yes.clone()],
            vec![text("system"), text("information_schema"), yes.clone()],
            vec![text("system"), text("main"), yes.clone()],
            vec![text("system"), text("pg_catalog"), yes.clone()],
            vec![text("temp"), text("main"), yes],
        ]
    );
    // The join a client writes, which is the whole reason these two carry numbers.
    assert_eq!(
        rows(
            &db,
            "SELECT count(*) FROM duckdb_schemas() s, duckdb_databases() d \
             WHERE s.database_oid = d.database_oid"
        ),
        vec![vec![Value::BigInt(5)]]
    );
}

/// The views a session has without making any, which is where `information_schema` comes from.
///
/// Every value here was read off the pin on the same statements, and the one thing that does not
/// match it is how many of these there are: upstream ships 47 and rudb ships the 12 whose bodies
/// only read table functions it has. See `rudb_catalog::system` for what the other 35 are waiting on.
#[test]
fn the_engine_ships_with_the_views_upstream_ships_with() {
    // A database of its own rather than the shared one, because what these views report is
    // everything in the catalog and the point of the test is that it is exactly what was made here.
    let db = Database::new();
    let text = |value: &str| Value::Varchar(value.to_string());
    db.execute("CREATE TABLE t(a INTEGER NOT NULL, b VARCHAR)").expect("a table to describe");
    db.execute("CREATE VIEW v AS SELECT a FROM t").expect("a view to describe");
    // The standard view of tables, which is a union of the two wrappers and so lists the table and
    // the view and nothing the engine owns.
    assert_eq!(
        rows(
            &db,
            "SELECT table_catalog, table_schema, table_name, table_type, is_insertable_into \
             FROM information_schema.tables ORDER BY table_name"
        ),
        vec![
            vec![text("memory"), text("main"), text("t"), text("BASE TABLE"), text("YES")],
            vec![text("memory"), text("main"), text("v"), text("VIEW"), text("NO")],
        ]
    );
    // A view's columns are listed next to a table's, and `is_nullable` is the word rather than the
    // boolean, which is the standard's spelling and the reason this view exists at all.
    assert_eq!(
        rows(
            &db,
            "SELECT table_name, column_name, ordinal_position, is_nullable, data_type \
             FROM information_schema.columns ORDER BY table_name, ordinal_position"
        ),
        vec![
            vec![text("t"), text("a"), Value::Integer(1), text("NO"), text("INTEGER")],
            vec![text("t"), text("b"), Value::Integer(2), text("YES"), text("VARCHAR")],
            vec![text("v"), text("a"), Value::Integer(1), text("YES"), text("INTEGER")],
        ]
    );
    assert_eq!(
        rows(
            &db,
            "SELECT character_set_name, default_collate_name FROM information_schema.character_sets"
        ),
        vec![vec![text("UTF8"), text("ucs_basic")]]
    );
    // The wrapper and the table function are different questions. The bare name is what somebody
    // made and the parentheses are what is really there, and the gap is the engine's own views.
    assert_eq!(rows(&db, "SELECT count(*) FROM duckdb_views"), vec![vec![Value::BigInt(1)]]);
    assert_eq!(rows(&db, "SELECT count(*) FROM duckdb_views()"), vec![vec![Value::BigInt(13)]]);
    // Nothing goes into a database the engine owns and nothing comes out of one.
    assert_eq!(
        failure(&db, "CREATE TABLE information_schema.x(a INTEGER)"),
        "Cannot create entry in system catalog"
    );
    // Through `execute` rather than `query`, because a drop answers nothing and the query path
    // complains about that before it gets as far as the catalog.
    assert_eq!(
        db.execute("DROP VIEW duckdb_views").expect_err("an internal entry").message(),
        "Cannot drop internal catalog entry \"duckdb_views\"!"
    );
}

/// The two pragmas that take a name, both of them answered while the query is bound.
///
/// Every row below was read off the pin on the same statements before it was written here.
#[test]
fn the_two_pragmas_describe_a_table_and_a_view_the_way_upstream_does() {
    // A database of its own, because these report the columns of whatever was made here and the
    // shared one has a second table in it that would only be noise.
    let db = Database::new();
    db.execute("CREATE TABLE t(a INTEGER NOT NULL, b VARCHAR)").expect("a table to describe");
    db.execute("CREATE VIEW v AS SELECT a FROM t").expect("a view to describe");
    let no = Value::Boolean(false);
    // `dflt_value` is null and `pk` is false on every row, because `CREATE TABLE` in rudb takes
    // neither a default nor a key yet. `cid` counts from zero, which is SQLite's numbering.
    let table = vec![
        vec![integer(0), text("a"), text("INTEGER"), Value::Boolean(true), Value::Null, no.clone()],
        vec![integer(1), text("b"), text("VARCHAR"), no.clone(), Value::Null, no.clone()],
    ];
    assert_eq!(rows(&db, "SELECT * FROM pragma_table_info('t')"), table);
    // The name is split under the identifier rule rather than taken whole, so a qualified name and
    // a fully qualified one in the wrong case are the same table.
    assert_eq!(rows(&db, "SELECT * FROM pragma_table_info('main.t')"), table);
    assert_eq!(rows(&db, "SELECT * FROM pragma_table_info('MEMORY.MAIN.T')"), table);
    // Every column of a view is nullable whatever the column underneath was declared as, so the
    // `NOT NULL` on `a` is gone by the time it is read through `v`.
    assert_eq!(
        rows(&db, "SELECT * FROM pragma_table_info('v')"),
        vec![vec![integer(0), text("a"), text("INTEGER"), no.clone(), Value::Null, no]]
    );
    // The same two columns again in the six `DESCRIBE` answers with, where nullability is the word
    // rather than the boolean and the sense of it is the other way round.
    assert_eq!(
        rows(&db, "SELECT * FROM pragma_show('t')"),
        vec![
            vec![text("a"), text("INTEGER"), text("NO"), Value::Null, Value::Null, Value::Null],
            vec![text("b"), text("VARCHAR"), text("YES"), Value::Null, Value::Null, Value::Null],
        ]
    );
    // An ordinary relation, which is the point of having these as functions and not only as
    // statements: an alias, a column list of its own, a qualified reference and a filter.
    assert_eq!(
        rows(
            &db,
            "SELECT info.name, info.type FROM pragma_table_info('t') AS info WHERE info.cid = 1"
        ),
        vec![vec![text("b"), text("VARCHAR")]]
    );
    assert_eq!(
        rows(&db, "SELECT n FROM pragma_table_info('t') AS info(c, n, ty, nn, d, k) WHERE c = 0"),
        vec![vec![text("a")]]
    );
}

/// Describing a view is reading it, including one the engine ships with.
#[test]
fn a_pragma_pointed_at_an_internal_view_binds_it_and_reports_its_columns() {
    let db = Database::new();
    let bound = "SELECT column_count FROM duckdb_views() WHERE view_name = 'duckdb_views'";
    assert_eq!(rows(&db, bound), vec![vec![Value::Null]]);
    assert_eq!(
        rows(&db, "SELECT count(*) FROM pragma_table_info('duckdb_views')"),
        vec![vec![Value::BigInt(13)]]
    );
    assert_eq!(rows(&db, bound), vec![vec![Value::BigInt(13)]]);
}

/// The four ways of getting one of these wrong, all four measured on the pin.
#[test]
fn a_pragma_given_a_name_that_is_not_there_says_what_the_catalog_says() {
    let db = database();
    assert_eq!(
        failure(&db, "SELECT * FROM pragma_table_info('nope')"),
        "Table with name nope does not exist!"
    );
    // A null is a name spelled `NULL` rather than a complaint about nulls, because the pin turns
    // whatever it was handed into text and then goes looking for a table called that.
    assert_eq!(
        failure(&db, "SELECT * FROM pragma_show(NULL)"),
        "Table with name NULL does not exist!"
    );
    for wrong in ["pragma_table_info()", "pragma_table_info('a', 'b')", "pragma_show(3)"] {
        let message = failure(&db, &format!("SELECT * FROM {wrong}"));
        assert!(message.starts_with("No function matches the given name"), "{message}");
        assert!(message.contains("(VARCHAR)"), "{message}");
    }
}

/// The four pragmas that report on the build and the process answer in one row each, in the columns
/// the pin names, and none of them takes anything.
#[test]
fn the_four_pragmas_about_the_build_answer_in_one_row_of_their_own_columns() {
    let db = database();
    let shapes = [
        ("pragma_version", vec!["library_version", "source_id", "codename"]),
        ("pragma_platform", vec!["platform"]),
        ("pragma_user_agent", vec!["user_agent"]),
        (
            "pragma_database_size",
            vec![
                "database_name",
                "database_size",
                "block_size",
                "total_blocks",
                "used_blocks",
                "free_blocks",
                "wal_size",
                "memory_usage",
                "memory_limit",
            ],
        ),
    ];
    for (name, columns) in shapes {
        let answer = rows(&db, &format!("SELECT * FROM {name}()"));
        assert_eq!(answer.len(), 1, "{name} answers about one build and one process");
        assert_eq!(answer[0].len(), columns.len(), "{name}");
        let named: Vec<Value> =
            rows(&db, &format!("SELECT column_name FROM (DESCRIBE SELECT * FROM {name}())"))
                .into_iter()
                .map(|row| row[0].clone())
                .collect();
        assert_eq!(named, columns.iter().map(|column| text(column)).collect::<Vec<Value>>());
        let message = failure(&db, &format!("SELECT * FROM {name}('t')"));
        assert!(message.starts_with("No function matches the given name"), "{message}");
        assert!(message.contains(&format!("\"{name}\"()")), "{message}");
    }
}

/// What the version pragma says is true of this engine rather than borrowed from the pin, and the
/// other two strings are built out of the same two facts.
#[test]
fn the_version_pragma_says_what_this_engine_is_and_the_others_agree_with_it() {
    let db = database();
    let version = rows(&db, "SELECT * FROM pragma_version()").remove(0);
    assert_eq!(version[0], text(&format!("v{}", env!("CARGO_PKG_VERSION"))));
    // A build nobody handed a revision to reports an empty source id, which is the honest answer,
    // and the codename is the one DuckDB itself uses before a release is named.
    assert_eq!(version[1], text(""));
    assert_eq!(version[2], text("Development Version"));
    let platform = rows(&db, "SELECT * FROM pragma_platform()").remove(0);
    let Value::Varchar(written) = &platform[0] else { panic!("the platform is text") };
    assert!(written.contains('_'), "{written}");
    assert!(!written.contains("macos") && !written.contains("x86_64"), "{written}");
    let agent = rows(&db, "SELECT * FROM pragma_user_agent()").remove(0);
    assert_eq!(agent[0], text(&format!("rudb/v{}({written})", env!("CARGO_PKG_VERSION"))));
}

/// The size pragma has a row for the one database that is attached and none for the two internal
/// ones, and the numbers are zero because nothing here is on disk.
#[test]
fn the_size_pragma_reports_the_attached_database_and_the_live_memory_budget() {
    let db = database();
    let size = rows(&db, "SELECT * FROM pragma_database_size()");
    assert_eq!(size.len(), 1);
    let row = &size[0];
    assert_eq!(row[0], text("memory"));
    for column in [1, 6] {
        assert_eq!(row[column], text("0 bytes"));
    }
    assert_eq!(
        row[2..=5],
        [Value::BigInt(0), Value::BigInt(0), Value::BigInt(0), Value::BigInt(0)]
    );
    // The last two are read off the budget rather than made up, so a limit somebody set shows up
    // here the same way it shows up in `current_setting`.
    db.execute("SET memory_limit = '1GiB'").expect("a size");
    let after = rows(&db, "SELECT memory_limit FROM pragma_database_size()").remove(0);
    assert_eq!(after[0], text("1.0 GiB"));
    assert_eq!(after[0], rows(&db, "SELECT current_setting('memory_limit')").remove(0)[0]);
}

/// `PRAGMA name` is the call it stands for, so the statement form answers what the function does.
#[test]
fn the_pragma_statement_answers_what_the_function_of_that_name_answers() {
    let db = database();
    for (statement, call) in [
        ("PRAGMA version", "SELECT * FROM pragma_version()"),
        ("PRAGMA platform", "SELECT * FROM pragma_platform()"),
        ("PRAGMA user_agent", "SELECT * FROM pragma_user_agent()"),
        ("PRAGMA database_size", "SELECT * FROM pragma_database_size()"),
        ("PRAGMA table_info('t')", "SELECT * FROM pragma_table_info('t')"),
        ("PRAGMA table_info(t)", "SELECT * FROM pragma_table_info('t')"),
        ("PRAGMA table_info(main.t)", "SELECT * FROM pragma_table_info('main.t')"),
        // The name is looked up without regard to case, the same way every other name is.
        ("PRAGMA VERSION", "SELECT * FROM pragma_version()"),
        // A view rather than a function, which the pragma namespace holds as well.
        ("PRAGMA database_list", "SELECT * FROM pragma_database_list"),
    ] {
        assert_eq!(rows(&db, statement), rows(&db, call), "{statement}");
    }
}

/// `PRAGMA name = value` is a `SET` written another way and it moves the same setting.
#[test]
fn a_pragma_with_an_equals_sign_sets_the_setting_a_plain_set_would() {
    let db = database();
    db.execute("PRAGMA memory_limit = '1GiB'").expect("a size");
    assert_eq!(rows(&db, "SELECT current_setting('memory_limit')"), vec![vec![text("1.0 GiB")]]);
    db.execute("PRAGMA threads = 4").expect("a count");
    assert_eq!(rows(&db, "SELECT current_setting('threads')"), vec![vec![Value::BigInt(4)]]);
}

/// What a pragma says when it is not one, and when it is one and was called wrongly.
#[test]
fn a_pragma_that_is_wrong_is_complained_about_in_the_spelling_it_was_written_in() {
    let db = database();
    // The name goes back out as it was typed, including its case, which is what the pin does.
    assert_eq!(failure(&db, "PRAGMA nope"), "Pragma Function with name nope does not exist!");
    assert_eq!(failure(&db, "PRAGMA NOPE"), "Pragma Function with name NOPE does not exist!");
    // A pragma that exists and was handed the wrong arguments is told about a pragma and not
    // about the `pragma_` name it was rewritten into, because the rewrite is not what was written.
    let message = failure(&db, "PRAGMA table_info");
    assert!(message.contains("'table_info()'"), "{message}");
    assert!(message.ends_with("\tPRAGMA \"table_info\"(VARCHAR)\n"), "{message}");
    let message = failure(&db, "PRAGMA table_info(1)");
    assert!(message.contains("'table_info(INTEGER)'"), "{message}");
    let message = failure(&db, "PRAGMA version(1)");
    assert!(message.ends_with("\tPRAGMA \"version\"\n"), "{message}");
}

/// A view the engine ships with goes in unbound and is bound at the first read, which is a fact
/// about two columns of `duckdb_views()` and was measured on the pin twice over.
#[test]
fn a_view_the_engine_ships_with_is_bound_when_it_is_first_read() {
    let db = database();
    let unbound = "SELECT column_count, is_bound FROM duckdb_views() WHERE view_name = 'schemata'";
    assert_eq!(rows(&db, unbound), vec![vec![Value::Null, Value::Boolean(false)]]);
    assert_eq!(
        rows(&db, "SELECT count(*) FROM information_schema.schemata"),
        vec![vec![Value::BigInt(5)]]
    );
    assert_eq!(rows(&db, unbound), vec![vec![Value::BigInt(7), Value::Boolean(true)]]);
}

#[test]
fn a_schema_carries_an_oid_of_its_own_and_not_its_databases() {
    let db = database();
    let rows = rows(&db, "SELECT oid, database_oid FROM duckdb_schemas()");
    assert_eq!(rows.len(), 5);
    for row in &rows {
        assert_ne!(row[0], row[1]);
        // Neither of them is the number a detached entry carries, which is what a caller reading
        // these through a join would silently collapse on.
        assert_ne!(row[0], Value::BigInt(0));
        assert_ne!(row[1], Value::BigInt(0));
    }
}

#[test]
fn the_tables_and_columns_tables_describe_what_was_created() {
    let db = database();
    let text = |value: &str| Value::Varchar(value.to_string());
    db.execute("CREATE TABLE shapes(x INTEGER, s VARCHAR, d DECIMAL(9,2), b BOOLEAN)")
        .expect("a fresh table");
    db.execute("INSERT INTO shapes VALUES (1, 'a', 1.5, true)").expect("a row");
    // Every value here is the pin's, read off it on the same statements. The `sql` column is the
    // interesting one: it is written back out from the entry rather than stored, which is why the
    // types come back upper case on a statement that did not write them that way.
    assert_eq!(
        rows(
            &db,
            "SELECT column_count, estimated_size, index_count, check_constraint_count, sql \
             FROM duckdb_tables() WHERE table_name = 'shapes'"
        ),
        vec![vec![
            Value::BigInt(4),
            Value::BigInt(1),
            Value::BigInt(0),
            Value::BigInt(0),
            text("CREATE TABLE shapes(x INTEGER, s VARCHAR, d DECIMAL(9,2), b BOOLEAN);"),
        ]]
    );
    assert_eq!(
        rows(
            &db,
            "SELECT column_name, column_index, data_type, data_type_id, numeric_precision, \
             numeric_precision_radix, numeric_scale FROM duckdb_columns() \
             WHERE table_name = 'shapes' ORDER BY column_index"
        ),
        vec![
            vec![
                text("x"),
                Value::Integer(1),
                text("INTEGER"),
                Value::BigInt(13),
                Value::Integer(32),
                Value::Integer(2),
                Value::Integer(0),
            ],
            vec![
                text("s"),
                Value::Integer(2),
                text("VARCHAR"),
                Value::BigInt(25),
                Value::Null,
                Value::Null,
                Value::Null,
            ],
            vec![
                text("d"),
                Value::Integer(3),
                text("DECIMAL(9,2)"),
                Value::BigInt(21),
                Value::Integer(9),
                Value::Integer(10),
                Value::Integer(2),
            ],
            vec![
                text("b"),
                Value::Integer(4),
                text("BOOLEAN"),
                Value::BigInt(10),
                Value::Null,
                Value::Null,
                Value::Null,
            ],
        ]
    );
}

#[test]
fn a_table_and_its_columns_agree_on_the_oid_they_join_on() {
    let db = database();
    let text = |value: &str| Value::Varchar(value.to_string());
    db.execute("CREATE TABLE joined(x INTEGER, s VARCHAR)").expect("a fresh table");
    // The join a client writes, and the reason every one of these tables carries an oid.
    assert_eq!(
        rows(
            &db,
            "SELECT c.column_name FROM duckdb_columns() c, duckdb_tables() t \
             WHERE c.table_oid = t.table_oid AND t.table_name = 'joined' ORDER BY c.column_index"
        ),
        vec![vec![text("x")], vec![text("s")]]
    );
}

#[test]
fn a_not_null_column_says_so_in_both_tables() {
    let db = database();
    let text = |value: &str| Value::Varchar(value.to_string());
    db.execute("CREATE TABLE n(a INTEGER NOT NULL, b INTEGER)").expect("a fresh table");
    assert_eq!(
        rows(&db, "SELECT sql FROM duckdb_tables() WHERE table_name = 'n'"),
        vec![vec![text("CREATE TABLE n(a INTEGER NOT NULL, b INTEGER);")]]
    );
    assert_eq!(
        rows(
            &db,
            "SELECT is_nullable FROM duckdb_columns() WHERE table_name = 'n' ORDER BY column_index"
        ),
        vec![vec![Value::Boolean(false)], vec![Value::Boolean(true)]]
    );
}

#[test]
fn a_view_lists_its_columns_the_way_a_table_does() {
    let db = database();
    let text = |value: &str| Value::Varchar(value.to_string());
    db.execute("CREATE TABLE base(x INTEGER NOT NULL, s VARCHAR, d DECIMAL(9,2))")
        .expect("a fresh table");
    db.execute("CREATE VIEW v AS SELECT x, s, d, x + 1 AS e FROM base").expect("a fresh view");
    db.execute("CREATE VIEW w(p, q) AS SELECT x, s FROM base").expect("a view with an alias list");
    // Every one of these was read off the pin. A computed column is reported as the type it comes
    // out as, the alias list is what the columns answer to, and `is_nullable` is true even on the
    // column that reads a NOT NULL column straight through.
    assert_eq!(
        rows(
            &db,
            "SELECT table_name, column_name, column_index, is_nullable, data_type \
             FROM duckdb_columns() WHERE table_name IN ('v', 'w') \
             ORDER BY table_name, column_index"
        ),
        vec![
            vec![text("v"), text("x"), Value::Integer(1), Value::Boolean(true), text("INTEGER")],
            vec![text("v"), text("s"), Value::Integer(2), Value::Boolean(true), text("VARCHAR")],
            vec![
                text("v"),
                text("d"),
                Value::Integer(3),
                Value::Boolean(true),
                text("DECIMAL(9,2)")
            ],
            vec![text("v"), text("e"), Value::Integer(4), Value::Boolean(true), text("INTEGER")],
            vec![text("w"), text("p"), Value::Integer(1), Value::Boolean(true), text("INTEGER")],
            vec![text("w"), text("q"), Value::Integer(2), Value::Boolean(true), text("VARCHAR")],
        ]
    );
    // A view's columns carry the view's oid rather than the oid of whatever is underneath it, which
    // is the join a client writes and the one that would silently return the wrong rows.
    assert_eq!(
        rows(
            &db,
            "SELECT count(*) FROM duckdb_columns() c, duckdb_tables() t \
             WHERE c.table_oid = t.table_oid AND c.table_name IN ('v', 'w')"
        ),
        vec![vec![Value::BigInt(0)]]
    );
}

/// Every row of the fifth catalog table, against what the pin answers for the same two views.
#[test]
fn a_view_reports_itself_and_the_statement_it_was_written_as() {
    let db = database();
    let text = |value: &str| Value::Varchar(value.to_string());
    db.execute("CREATE TABLE base(x INTEGER, s VARCHAR)").expect("a fresh table");
    db.execute("CREATE VIEW v AS SELECT x, s FROM base WHERE x > 0").expect("a fresh view");
    // Odd spacing, a comment, a lower case keyword and a name in the wrong case, none of which
    // survives into the column. What the pin reports is the statement written back out, so the
    // comment goes, the spacing is normalised, the keywords come back upper case and the names come
    // back in the case they were written in.
    db.execute("CREATE VIEW w(p) AS -- a comment\n   select   X  as Y from  base")
        .expect("a view with an alias list");
    assert_eq!(
        rows(
            &db,
            "SELECT view_name, column_count, internal, temporary, is_bound, sql \
             FROM duckdb_views() WHERE view_name IN ('v', 'w') ORDER BY view_name"
        ),
        vec![
            vec![
                text("v"),
                Value::BigInt(2),
                Value::Boolean(false),
                Value::Boolean(false),
                Value::Boolean(true),
                text("CREATE VIEW v AS SELECT x, s FROM base WHERE (x > 0);"),
            ],
            vec![
                text("w"),
                Value::BigInt(1),
                Value::Boolean(false),
                Value::Boolean(false),
                Value::Boolean(true),
                text("CREATE VIEW w (p) AS SELECT X AS Y FROM base;"),
            ],
        ]
    );
    // The join a client writes, which is the whole point of the oid columns being filled in.
    assert_eq!(
        rows(
            &db,
            "SELECT count(*) FROM duckdb_views() v, duckdb_columns() c \
             WHERE v.view_oid = c.table_oid"
        ),
        vec![vec![Value::BigInt(3)]]
    );
    // And a table is not a view, so neither table lists what the other one does.
    assert!(rows(&db, "SELECT view_name FROM duckdb_views() WHERE view_name = 'base'").is_empty());
    assert!(rows(&db, "SELECT table_name FROM duckdb_tables() WHERE table_name = 'v'").is_empty());
}

/// The two tables a client reads on connect to find out what engine it got.
#[test]
fn the_engine_answers_for_its_optimizer_passes_and_its_extensions() {
    let db = database();
    let text = |value: &str| Value::Varchar(value.to_string());
    // Forty four on the pin and forty four here, because the table is the set of names
    // `SET disabled_optimizers` takes and rudb takes every one of them.
    assert_eq!(
        rows(&db, "SELECT count(*) FROM duckdb_optimizers()"),
        vec![vec![Value::BigInt(44)]]
    );
    // The eight rudb has actually written are all in that list rather than names of its own, which
    // is what makes a corpus file written against DuckDB turn off the pass it meant.
    assert_eq!(
        rows(
            &db,
            "SELECT count(*) FROM duckdb_optimizers() WHERE name IN ('expression_rewriter', \
             'distinct_aggregate_rewrite', 'filter_pushdown', 'empty_result_pullup', \
             'unused_columns', 'limit_pushdown', 'top_n', 'late_materialization')"
        ),
        vec![vec![Value::BigInt(8)]]
    );
    // And the name the table gives is a name the setting takes.
    db.execute("SET disabled_optimizers = 'join_order,filter_pushdown'").expect("both names take");
    // Thirty one extensions on the pin and thirty one here. Two of them are true here where six are
    // true there, and the two are the two rudb has.
    assert_eq!(
        rows(&db, "SELECT count(*) FROM duckdb_extensions()"),
        vec![vec![Value::BigInt(31)]]
    );
    assert_eq!(
        rows(&db, "SELECT extension_name FROM duckdb_extensions() WHERE loaded ORDER BY 1"),
        vec![vec![text("core_functions")], vec![text("parquet")]]
    );
    assert_eq!(
        rows(
            &db,
            "SELECT loaded, installed, install_path, install_mode, signature_key_fingerprint \
             FROM duckdb_extensions() WHERE extension_name = 'parquet'"
        ),
        vec![vec![
            Value::Boolean(true),
            Value::Boolean(true),
            text("(BUILT-IN)"),
            text("STATICALLY_LINKED"),
            Value::Null,
        ]]
    );
    // The three empty strings on a row for something that is not here are empty strings and not
    // nulls, which was measured, and the fingerprint is null on every row either way.
    assert_eq!(
        rows(
            &db,
            "SELECT loaded, installed, install_path, extension_version, install_mode, \
             installed_from FROM duckdb_extensions() WHERE extension_name = 'spatial'"
        ),
        vec![vec![
            Value::Boolean(false),
            Value::Boolean(false),
            text(""),
            text(""),
            text("NOT_INSTALLED"),
            text(""),
        ]]
    );
    // The aliases are the pin's, because they are a fact about the extension rather than about this
    // engine, and an extension with none carries an empty list rather than a null.
    let aliases = |values: Vec<&str>| Value::List {
        element: LogicalType::Varchar,
        values: values.into_iter().map(text).collect(),
    };
    assert_eq!(
        rows(
            &db,
            "SELECT aliases FROM duckdb_extensions() \
             WHERE extension_name IN ('httpfs', 'parquet') ORDER BY extension_name"
        ),
        vec![vec![aliases(vec!["http", "https", "s3"])], vec![aliases(Vec::new())]]
    );
    // Both tables are table functions, so both list themselves in the function table.
    assert_eq!(
        rows(
            &db,
            "SELECT count(*) FROM duckdb_functions() \
             WHERE function_name IN ('duckdb_extensions', 'duckdb_optimizers')"
        ),
        vec![vec![Value::BigInt(2)]]
    );
}

#[test]
fn the_engine_lists_its_parser_dialect_and_no_grammar_extensions() {
    let db = database();
    assert_eq!(rows(&db, "SELECT * FROM duckdb_dialects()"), vec![vec![text("duckdb")]]);
    assert!(rows(&db, "SELECT * FROM duckdb_grammar_extensions()").is_empty());
    assert_eq!(
        rows(
            &db,
            "SELECT column_name, column_type FROM (DESCRIBE SELECT * FROM duckdb_grammar_extensions())"
        ),
        vec![vec![text("name"), text("VARCHAR")], vec![text("description"), text("VARCHAR")]]
    );
    assert_eq!(
        rows(
            &db,
            "SELECT count(*) FROM duckdb_functions() WHERE function_name IN ('duckdb_dialects', 'duckdb_grammar_extensions')"
        ),
        vec![vec![Value::BigInt(2)]]
    );
}

/// A view over a star follows the table under it, and the column list follows with it.
#[test]
fn a_star_in_a_view_is_expanded_again_every_time_the_view_is_read() {
    let db = database();
    let text = |value: &str| Value::Varchar(value.to_string());
    db.execute("CREATE TABLE base(x INTEGER)").expect("a fresh table");
    db.execute("CREATE VIEW star AS SELECT * FROM base").expect("a fresh view");
    assert_eq!(
        rows(&db, "SELECT column_name FROM duckdb_columns() WHERE table_name = 'star'"),
        vec![vec![text("x")]]
    );
    // Reading the view rewrites the list. There is no ALTER TABLE yet, so this cannot yet be made
    // to report a different answer than it did before, and the point of it here is that a read does
    // not damage the list either.
    db.query("SELECT * FROM star").expect("the view reads");
    assert_eq!(
        rows(&db, "SELECT column_name FROM duckdb_columns() WHERE table_name = 'star'"),
        vec![vec![text("x")]]
    );
}

/// The four ways a subscript is refused, in DuckDB's words. Per #278.
#[test]
fn the_subscripts_that_are_refused_say_what_duckdb_says() {
    let db = database();
    let error = db.query("SELECT 'abcdef'[]").unwrap_err();
    assert_eq!(error.code().duckdb_name(), "Parser Error");
    assert_eq!(error.message(), "Empty subscript '[]' is not allowed");
    // A step on a string is not implemented upstream either, and the suggested rewrite is
    // upstream's, unbalanced parenthesis and all.
    let error = db.query("SELECT 'abcdef'[1:6:2]").unwrap_err();
    assert_eq!(error.code().duckdb_name(), "Not implemented Error");
    assert!(error.message().starts_with("Slice with steps has not been implemented"), "{error}");
    // A number is neither a list nor a string, and this is the one message that names the function
    // rather than listing what it would have taken.
    let error = db.query("SELECT array_slice(1, 2, 3)").unwrap_err();
    assert_eq!(error.code().duckdb_name(), "Binder Error");
    assert_eq!(error.message(), "ARRAY_SLICE can only operate on LISTs and VARCHARs");
    // An index is a whole number and is not cast to one, so a decimal is no call at all.
    assert!(failure(&db, "SELECT 'abcdef'[1.5]").starts_with("No function matches"));
}

/// A dollar quoted string is the text between the tags and nothing else. Per #276.
///
/// The comparison is the test worth having. The literal on its own looked plausible in the shell
/// output, and what the bug really did was make a dollar quoted string unequal to the same string
/// written the ordinary way, with nothing raising and nothing looking odd in the plan.
#[test]
fn a_dollar_quoted_string_is_the_text_between_the_tags() {
    let db = Database::new();
    assert_eq!(rows(&db, "SELECT $$dollar quoted$$"), vec![vec![text("dollar quoted")]]);
    assert_eq!(rows(&db, "SELECT $tag$body$tag$"), vec![vec![text("body")]]);
    assert_eq!(rows(&db, "SELECT $$a$$ = 'a'"), vec![vec![Value::Boolean(true)]]);
    // The name a column gets is rendered from the value, so it comes right with it.
    assert_eq!(db.query("SELECT $$a$$").unwrap().names(), &["'a'".to_string()]);
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

/// What a failed cast says, per #322, which is a different sentence for each shape of failure and
/// names the types by the integer they are stored in rather than by the way they are written.
///
/// A pair DuckDB has no cast for is a conversion error and not a missing feature, which is why the
/// last line here answers null instead of raising: `TRY_CAST` swallows a conversion error.
#[test]
fn a_failed_cast_says_what_duckdb_says() {
    let db = database();
    assert_eq!(failure(&db, "SELECT 'abc'::TINYINT"), "Could not convert string 'abc' to INT8");
    assert_eq!(failure(&db, "SELECT '300'::TINYINT"), "Could not convert string '300' to INT8");
    assert_eq!(
        failure(&db, "SELECT 'abc'::DECIMAL(4,1)"),
        "Could not convert string \"abc\" to DECIMAL(4,1)"
    );
    assert_eq!(
        failure(&db, "SELECT 300::INTEGER::TINYINT"),
        "Type INT32 with value 300 can't be cast because the value is out of range for the \
         destination type INT8"
    );
    assert_eq!(
        failure(&db, "SELECT 999.9::DECIMAL(4,1)::TINYINT"),
        "Failed to cast decimal value 1000 to type INT8"
    );
    assert_eq!(
        failure(&db, "SELECT 200000::DECIMAL(4,1)"),
        "Could not cast value 200000 to DECIMAL(4,1)"
    );
    assert_eq!(
        failure(&db, "SELECT 200000.5::DECIMAL(7,1)::DECIMAL(4,1)"),
        "Casting value \"200000.5\" to type DECIMAL(4,1) failed: value is out of range!"
    );
    assert_eq!(
        failure(&db, "SELECT DATE '1970-01-01'::INTEGER"),
        "Unimplemented type for cast (DATE -> INTEGER)"
    );
    assert_eq!(rows(&db, "SELECT TRY_CAST(DATE '1970-01-01' AS INTEGER)"), vec![vec![Value::Null]]);
}

/// A written date that names a day that does not exist, per #322.
///
/// This one was a wrong answer and not only a wrong message: the thirty first of April used to
/// come back as the first of May. The time on the end of a date is thrown away but still has to be
/// a time, and the two sentences are the format one and the range one.
#[test]
fn a_written_day_that_does_not_exist_is_refused() {
    let db = database();
    assert_eq!(
        failure(&db, "SELECT '2021-04-31'::DATE"),
        "date field value out of range: \"2021-04-31\""
    );
    assert_eq!(
        failure(&db, "SELECT '2021-02-29 10:00:00'::TIMESTAMP"),
        "timestamp field value out of range: \"2021-02-29 10:00:00\""
    );
    assert_eq!(
        failure(&db, "SELECT 'yesterday'::DATE"),
        "invalid date field format: \"yesterday\", expected format is (YYYY-MM-DD)"
    );
    assert_eq!(
        rows(&db, "SELECT '2020-02-29 10:30:00'::DATE"),
        vec![vec![Value::Date(days_from_civil(2020, 2, 29))]]
    );
}

/// A TIME can be made now, per #228, which was a type you could declare a column of and never put
/// a value in.
///
/// The written form is read with a lot more slack than a date or a timestamp is, which is upstream
/// and not a decision here: the seconds are optional, a date in front is thrown away, and anything
/// after the numbers is ignored. The printing is the other half, since a TIME prints the fraction
/// it has and no more, so `12:34:56.100` comes back as `12:34:56.1`.
#[test]
fn a_time_can_be_written_and_read_back() {
    let db = database();
    let at = |hours: i64, minutes: i64, seconds: i64, micros: i64| {
        vec![vec![Value::Time(((hours * 60 + minutes) * 60 + seconds) * 1_000_000 + micros)]]
    };
    assert_eq!(rows(&db, "SELECT TIME '12:34:56'"), at(12, 34, 56, 0));
    assert_eq!(rows(&db, "SELECT CAST('12:34:56' AS TIME)"), at(12, 34, 56, 0));
    assert_eq!(rows(&db, "SELECT '12:34'::TIME"), at(12, 34, 0, 0));
    assert_eq!(rows(&db, "SELECT '12:34:56.1234567'::TIME"), at(12, 34, 56, 123_456));
    assert_eq!(rows(&db, "SELECT '2024-01-02 03:04:05'::TIME"), at(3, 4, 5, 0));
    assert_eq!(rows(&db, "SELECT '12:34:56 UTC'::TIME"), at(12, 34, 56, 0));
    assert_eq!(rows(&db, "SELECT TIMESTAMP '2024-01-02 03:04:05'::TIME"), at(3, 4, 5, 0));
    assert_eq!(rows(&db, "SELECT '12:34:56.100'::TIME::VARCHAR"), vec![vec![text("12:34:56.1")]]);
    assert_eq!(rows(&db, "SELECT typeof(TIME '12:34:56')"), vec![vec![text("TIME")]]);
    assert_eq!(
        failure(&db, "SELECT '25:00:00'::TIME"),
        "time field value out of range: \"25:00:00\", expected format is ([YYYY-MM-DD ]HH:MM:SS[.MS])"
    );
    assert_eq!(rows(&db, "SELECT TRY_CAST('25:00:00' AS TIME)"), vec![vec![Value::Null]]);
    assert_eq!(
        failure(&db, "SELECT DATE '2024-01-02'::TIME"),
        "Unimplemented type for cast (DATE -> TIME)"
    );
}

/// The third spelling of a cast, the one TPC-H q14 is written in.
///
/// The keyword is not a keyword the way `CAST` is, it is the type name, so the check that matters
/// is that the same list of types works here as works in the other two spellings. The name of the
/// column is the same too, because it is the same node: the pinned binary calls all three of them
/// `CAST('1995-09-01' AS DATE)`.
#[test]
fn a_type_in_front_of_a_string_is_a_cast_of_that_string() {
    let db = database();
    assert_eq!(
        rows(&db, "SELECT DATE '2013-07-15'"),
        vec![vec![Value::Date(days_from_civil(2013, 7, 15))]]
    );
    assert_eq!(
        rows(&db, "SELECT date '2013-07-15'"),
        vec![vec![Value::Date(days_from_civil(2013, 7, 15))]],
        "the type is a name and names are not case sensitive"
    );
    assert_eq!(rows(&db, "SELECT INTEGER '42' + 1"), vec![vec![Value::Integer(43)]]);
    assert_eq!(rows(&db, "SELECT VARCHAR 'hi'"), vec![vec![text("hi")]]);
    let result = db.query("SELECT DATE '1995-09-01'").unwrap();
    assert_eq!(result.names(), &["CAST('1995-09-01' AS DATE)"]);
    assert!(failure(&db, "SELECT DATE 'nope'").contains("nope"));
}

/// A prefix in front of a string picks a different decoding, per #329.
///
/// The prefix is not part of the value and never was, so the thing to check end to end is that the
/// value is the decoded one and the column name is the name the pinned binary gives it. The escapes
/// themselves are checked one at a time where they are decoded, in the parser.
#[test]
fn a_prefix_in_front_of_a_string_decides_how_the_string_is_read() {
    let db = database();
    assert_eq!(rows(&db, "SELECT E'a\\tb'"), vec![vec![text("a\tb")]]);
    assert_eq!(rows(&db, "SELECT e'a\\u00e9b'"), vec![vec![text("aéb")]]);
    assert_eq!(rows(&db, "SELECT N'abc'"), vec![vec![text("abc")]]);
    assert_eq!(rows(&db, "SELECT B'101'"), vec![vec![text("b101")]], "not a bit string upstream");
    assert_eq!(rows(&db, "SELECT length(E'a\\nb')"), vec![vec![Value::BigInt(3)]]);
    // An escape string is named after the value and not the spelling, so it ends up with the name a
    // plain string of the same value has. An N string is named as the cast it is.
    let result = db.query("SELECT E'ab', N'ab'").unwrap();
    assert_eq!(result.names(), &["'ab'", "CAST('ab' AS VARCHAR)"]);
}

/// A hex string is a blob, per #329, which is the prefix that changes the type and not the value.
#[test]
fn a_hex_string_answers_with_the_bytes_it_names() {
    let db = database();
    assert_eq!(rows(&db, "SELECT x'ff'"), vec![vec![Value::Blob(vec![0xff])]]);
    assert_eq!(rows(&db, "SELECT X'4142'"), vec![vec![Value::Blob(b"AB".to_vec())]]);
    assert_eq!(rows(&db, "SELECT x''"), vec![vec![Value::Blob(Vec::new())]]);
    // The same blob written the other way round, which is the cast this literal is built on.
    assert_eq!(rows(&db, "SELECT x'ff' = '\\xFF'::BLOB"), vec![vec![Value::Boolean(true)]]);
    let result = db.query("SELECT x'ff41'").unwrap();
    assert_eq!(result.names(), &["'\\xFFA'::BLOB"]);
    assert_eq!(result.types(), &[LogicalType::Blob]);
    // The digits are not looked at until the cast, so this is a conversion error and not a syntax
    // one, and it is the message the same cast written the other way round gives.
    assert!(failure(&db, "SELECT x'41zz'").contains("string -> blob conversion of string"));
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

/// A predicate nothing can satisfy is answered without reading the table. The plan is the assertion
/// rather than the timing, because the whole point is that the scan is not in it, and with the pass
/// turned off the same query has to give the same answer the slow way.
#[test]
fn a_query_that_cannot_match_answers_nothing_without_reading_the_table() {
    let db = database();
    let text = db.plan("SELECT x FROM t WHERE false").unwrap();
    assert!(!text.contains("Get memory.main.t"), "{text}");
    assert_eq!(rows(&db, "SELECT x FROM t WHERE false"), Vec::<Vec<Value>>::new());
    assert_eq!(rows(&db, "SELECT x FROM t LIMIT 0"), Vec::<Vec<Value>>::new());
    db.execute("SET disabled_optimizers = 'empty_result_pullup'").unwrap();
    let text = db.plan("SELECT x FROM t WHERE false").unwrap();
    assert!(text.contains("Get memory.main.t"), "{text}");
    assert_eq!(rows(&db, "SELECT x FROM t WHERE false"), Vec::<Vec<Value>>::new());
    assert_eq!(rows(&db, "SELECT x FROM t LIMIT 0"), Vec::<Vec<Value>>::new());
}

/// The case the pullup has to stop at. An ungrouped aggregate over no rows produces one row, so
/// `count(*)` of nothing is zero and not an empty answer, and a `min` of nothing is null.
#[test]
fn an_aggregate_over_a_query_that_cannot_match_still_answers_its_row() {
    let db = database();
    assert_eq!(
        rows(&db, "SELECT count(*), min(x) FROM t WHERE false"),
        vec![vec![Value::BigInt(0), Value::Null]]
    );
    assert_eq!(
        rows(&db, "SELECT x, count(*) FROM t WHERE false GROUP BY x"),
        Vec::<Vec<Value>>::new()
    );
    db.execute("SET disabled_optimizers = 'empty_result_pullup'").unwrap();
    assert_eq!(
        rows(&db, "SELECT count(*), min(x) FROM t WHERE false"),
        vec![vec![Value::BigInt(0), Value::Null]]
    );
}

/// The other half of constant pruning. A predicate that is always true keeps every row, so the
/// filter is not rebuilt at all rather than left to be evaluated once per row to say so.
#[test]
fn a_predicate_that_is_always_true_leaves_no_filter_behind() {
    let db = database();
    let text = db.plan("SELECT x FROM t WHERE true").unwrap();
    assert!(!text.contains("Filter"), "{text}");
    let text = db.plan("SELECT x FROM t WHERE true AND x > 1").unwrap();
    assert!(text.contains("Filter (#0.0::INTEGER > 1::INTEGER)::BOOLEAN"), "{text}");
    assert_eq!(
        rows(&db, "SELECT x FROM t WHERE true AND x > 1"),
        vec![vec![integer(3)], vec![integer(2)]]
    );
}

/// A limit ends up under the projection above it, so the expressions are evaluated for the rows
/// that come out rather than for a whole chunk of rows that were going to be dropped. The rows are
/// the same either way, which is the half of this worth asserting, and the plan is the other half.
#[test]
fn a_limit_ends_up_under_the_projection_and_answers_the_same_rows() {
    let db = database();
    let text = db.plan("SELECT x + 1 AS y FROM t LIMIT 2").unwrap();
    let lines: Vec<&str> = text.lines().collect();
    assert!(lines[0].starts_with("Project"), "{text}");
    assert!(lines[1].trim_start().starts_with("Limit 2 offset 0"), "{text}");
    let wanted = vec![vec![integer(4)], vec![integer(2)]];
    assert_eq!(rows(&db, "SELECT x + 1 AS y FROM t LIMIT 2"), wanted);
    db.execute("SET disabled_optimizers = 'limit_pushdown'").unwrap();
    let text = db.plan("SELECT x + 1 AS y FROM t LIMIT 2").unwrap();
    assert!(text.lines().next().unwrap().starts_with("Limit 2 offset 0"), "{text}");
    assert_eq!(rows(&db, "SELECT x + 1 AS y FROM t LIMIT 2"), wanted);
}

/// The pass runs before top N, so an order by with a limit is still fused rather than left as a
/// sort with a limit that has wandered off above it.
#[test]
fn an_order_by_with_a_limit_is_still_one_operator_after_the_limit_has_moved() {
    let db = database();
    let printed = db.plan("SELECT s FROM t ORDER BY x DESC LIMIT 2").unwrap();
    assert!(printed.contains("TopN 2 offset 0"), "{printed}");
    assert_eq!(
        rows(&db, "SELECT s FROM t ORDER BY x DESC LIMIT 2"),
        vec![vec![text("a")], vec![text("c")]]
    );
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

/// `1.5 * 1.5` is 2.25, which needs the two decimal places a product of two of them has.
///
/// It used to answer 2.2 typed `DECIMAL(2,1)`, because the result kept what the operands promote
/// to and the last digit of the answer had nowhere to go. Per #243. The second query is the same
/// rule through the kernel rather than a value at a time, since a column of them takes the run at a
/// time path and that one multiplies unscaled values at their own scales.
#[test]
fn multiplying_two_decimals_keeps_the_digits_of_both_of_them() {
    let db = Database::new();
    let sql = "SELECT 1.5 * 1.5 AS p";
    assert_eq!(db.query(sql).unwrap().types(), &[LogicalType::Decimal { width: 4, scale: 2 }]);
    assert_eq!(rows(&db, sql), vec![vec![Value::Decimal { unscaled: 225, width: 4, scale: 2 }]]);
    let run = "SELECT a * 1.5 AS p FROM range(3) t(a)";
    assert_eq!(db.query(run).unwrap().types(), &[LogicalType::Decimal { width: 21, scale: 1 }]);
    let decimal = |unscaled| vec![Value::Decimal { unscaled, width: 21, scale: 1 }];
    assert_eq!(rows(&db, run), vec![decimal(0), decimal(15), decimal(30)]);
}

/// `//` is integer division only when there are integers on both sides of it.
///
/// Measured on the pinned binary: `7.5 // 2.5` is the DOUBLE 3.0, `7.5 // 2` is 3.75 and
/// `7.9 // 1.0` is 7.9, so once a side is not an integer it is `/` under another spelling and it
/// neither keeps the decimal type nor truncates the answer.
#[test]
fn integer_division_with_a_decimal_in_it_is_a_double() {
    let db = Database::new();
    let sql = "SELECT 7.5 // 2.5 AS q";
    assert_eq!(db.query(sql).unwrap().types(), &[LogicalType::Double]);
    assert_eq!(rows(&db, sql), vec![vec![Value::Double(3.0)]]);
    let sql = "SELECT 7.5 // 2 AS q";
    assert_eq!(db.query(sql).unwrap().types(), &[LogicalType::Double]);
    assert_eq!(rows(&db, sql), vec![vec![Value::Double(3.75)]]);
    let sql = "SELECT 7.9 // 1.0 AS q";
    assert_eq!(rows(&db, sql), vec![vec![Value::Double(7.9)]]);
    let sql = "SELECT 7 // 2 AS q";
    assert_eq!(db.query(sql).unwrap().types(), &[LogicalType::Integer]);
    assert_eq!(rows(&db, sql), vec![vec![Value::Integer(3)]]);
    // A run rather than one value, so the vectorized loop answers the same as the folded constant.
    let run = "SELECT a // 2.0 AS q FROM range(4) t(a)";
    assert_eq!(db.query(run).unwrap().types(), &[LogicalType::Double]);
    let double = |value| vec![Value::Double(value)];
    assert_eq!(rows(&db, run), vec![double(0.0), double(0.5), double(1.0), double(1.5)]);
}

/// Every overflow sentence, word for word and mark for mark against the pinned binary. Per #257.
///
/// The type is the integer the value is stored in rather than the type it was written as, a decimal
/// prints its operands with the point taken out, an integer ends on `!` and a decimal on `;`, a
/// decimal subtraction is called `subtract`, and a decimal multiplication ends on advice about how
/// to get out of the overflow instead of on punctuation.
#[test]
fn an_overflow_says_what_duckdb_says() {
    let db = Database::new();
    let cases = [
        ("127::TINYINT + 1::TINYINT", "Overflow in addition of INT8 (127 + 1)!"),
        ("(-128)::TINYINT - 1::TINYINT", "Overflow in subtraction of INT8 (-128 - 1)!"),
        ("127::TINYINT * 2::TINYINT", "Overflow in multiplication of INT8 (127 * 2)!"),
        ("32767::SMALLINT + 1::SMALLINT", "Overflow in addition of INT16 (32767 + 1)!"),
        ("2147483647::INTEGER + 1::INTEGER", "Overflow in addition of INT32 (2147483647 + 1)!"),
        (
            "9223372036854775807::BIGINT + 1::BIGINT",
            "Overflow in addition of INT64 (9223372036854775807 + 1)!",
        ),
        ("255::UTINYINT + 1::UTINYINT", "Overflow in addition of UINT8 (255 + 1)!"),
        ("65535::USMALLINT + 1::USMALLINT", "Overflow in addition of UINT16 (65535 + 1)!"),
        ("4294967295::UINTEGER + 1::UINTEGER", "Overflow in addition of UINT32 (4294967295 + 1)!"),
        (
            "18446744073709551615::UBIGINT + 1::UBIGINT",
            "Overflow in addition of UINT64 (18446744073709551615 + 1)!",
        ),
        (
            "170141183460469231731687303715884105727::HUGEINT + 1::HUGEINT",
            "Overflow in addition of INT128 (170141183460469231731687303715884105727 + 1)!",
        ),
        (
            "99999999999999999999999999999999999999::DECIMAL(38,0) + 1::DECIMAL(38,0)",
            "Overflow in addition of DECIMAL(38) (99999999999999999999999999999999999999 + 1);",
        ),
        (
            "(-99999999999999999999999999999999999999)::DECIMAL(38,0) - 1::DECIMAL(38,0)",
            "Overflow in subtract of DECIMAL(38) (-99999999999999999999999999999999999999 - 1);",
        ),
        (
            "9999999999.99::DECIMAL(38,2) * 9999999999999999999999999999.99::DECIMAL(38,2)",
            concat!(
                "Overflow in multiplication of DECIMAL(38) ",
                "(999999999999 * 999999999999999999999999999999). ",
                "You might want to add an explicit cast to a decimal with a smaller scale.",
            ),
        ),
        (
            "9999999999.9999::DECIMAL(18,4) * 99999999.9999::DECIMAL(18,4)",
            concat!(
                "Overflow in multiplication of DECIMAL(18) (99999999999999 * 999999999999). ",
                "You might want to add an explicit cast to a bigger decimal.",
            ),
        ),
        ("abs((-2147483648)::INTEGER)", "Overflow on abs(-2147483648)"),
        ("abs((-32768)::SMALLINT)", "Overflow on abs(-32768)"),
        ("abs((-9223372036854775808)::BIGINT)", "Overflow on abs(-9223372036854775808)"),
        // Negation names neither the type nor the value, per #264. A HUGEINT is the only width that
        // reaches it as a constant, because the folder widens the other four instead.
        (
            "-((-170141183460469231731687303715884105728)::HUGEINT)",
            "Overflow in negation of numeric value!",
        ),
    ];
    for (expression, expected) in cases {
        assert_eq!(failure(&db, &format!("SELECT {expression}")), expected, "{expression}");
    }
    // The vectorized loop, which is a second copy of each of these messages and has to say the same
    // thing. One operand comes out of a column so that the constant folder leaves the row alone.
    let one = "FROM range(1, 2) t(a)";
    let sql = format!("SELECT 127::TINYINT + a::TINYINT {one}");
    assert_eq!(failure(&db, &sql), "Overflow in addition of INT8 (127 + 1)!");
    let big = "99999999999999999999999999999999999999::DECIMAL(38,0)";
    let sql = format!("SELECT {big} + a::DECIMAL(38,0) {one}");
    let expected =
        "Overflow in addition of DECIMAL(38) (99999999999999999999999999999999999999 + 1);";
    assert_eq!(failure(&db, &sql), expected);
    let sql = "SELECT abs(a::INTEGER) FROM range(-2147483648, -2147483647) t(a)";
    assert_eq!(failure(&db, sql), "Overflow on abs(-2147483648)");
    let sql = "SELECT -(a::INTEGER) FROM range(-2147483648, -2147483647) t(a)";
    assert_eq!(failure(&db, sql), "Overflow in negation of numeric value!");
}

/// Negating the smallest value of a signed type widens by a step instead of raising. Per #264.
///
/// The type of the answer depends on the value, which is why it is the constant folder that does it
/// and why a column keeps the type it has: nothing knows what is in the column until the loop is
/// running, and upstream keeps the type there too. Measured on the pinned binary, where
/// `typeof(-((-128)::TINYINT))` is SMALLINT and `typeof(-((-127)::TINYINT))` is TINYINT.
#[test]
fn negating_the_smallest_value_of_a_type_widens_by_one_step() {
    let db = Database::new();
    let widened = [
        ("(-128)::TINYINT", LogicalType::SmallInt, Value::SmallInt(128)),
        ("(-32768)::SMALLINT", LogicalType::Integer, Value::Integer(32768)),
        ("(-2147483648)::INTEGER", LogicalType::BigInt, Value::BigInt(2147483648)),
        (
            "(-9223372036854775808)::BIGINT",
            LogicalType::HugeInt,
            Value::HugeInt(9223372036854775808),
        ),
    ];
    for (argument, ty, answer) in widened {
        let sql = format!("SELECT -({argument}) AS n");
        assert_eq!(db.query(&sql).unwrap().types(), &[ty], "{argument}");
        assert_eq!(rows(&db, &sql), vec![vec![answer]], "{argument}");
    }
    // Every other value keeps the type it was written with, so the widening is about the one value
    // in each type that has no negative and not about the type.
    let sql = "SELECT -((-127)::TINYINT) AS n";
    assert_eq!(db.query(sql).unwrap().types(), &[LogicalType::TinyInt]);
    assert_eq!(rows(&db, sql), vec![vec![Value::TinyInt(127)]]);
    // A column keeps its type as well, and the row that has no negative in it raises instead.
    let sql = "SELECT -(a::INTEGER) AS n FROM range(-2147483647, -2147483646) t(a)";
    assert_eq!(db.query(sql).unwrap().types(), &[LogicalType::Integer]);
    assert_eq!(rows(&db, sql), vec![vec![Value::Integer(2147483647)]]);
}

/// Something written where a number goes that is not one, and the three answers upstream has. Per
/// #277.
///
/// `SELECT 1e` is a refusal on the pinned binary as well, which is worth writing down because it
/// looks like it should be `1` aliased `e`. It is that one byte later. The tokenizer gives the
/// exponent marker back when there is anything at all behind it to give it back into, and at the end
/// of the input there is not. So what was wrong here was the class and the words rather than the
/// refusal, and all four of these were read off the binary.
#[test]
fn a_number_that_is_not_a_number_says_what_duckdb_says() {
    let db = Database::new();
    let cases = [
        ("SELECT 1e", "Invalid Input Error", "Could not convert string '1e' to DOUBLE"),
        ("SELECT 1e-", "Invalid Input Error", "Could not convert string '1e-' to DOUBLE"),
        ("SELECT 1e2e", "Parser Error", "Already found scientific notation"),
        (
            "SELECT 1.2.3",
            "Invalid Input Error",
            "Failed to cast value: Could not convert string \"1.2.3\" to DECIMAL(4,1)",
        ),
    ];
    for (sql, class, message) in cases {
        let error = db.query(sql).unwrap_err();
        assert_eq!(error.code().duckdb_name(), class, "{sql}");
        assert_eq!(error.message(), message, "{sql}");
    }
}

/// An underscore between two digits is a separator and not part of the number. Per #277.
///
/// The tokenizer has taken these as one number token since it was written, and the binder then
/// refused every one of them, so nothing could be written with a separator in it at all. The type
/// counts digits, so the separator has to be gone before the counting rather than after it.
#[test]
fn an_underscore_in_a_number_is_a_separator() {
    let db = Database::new();
    assert_eq!(rows(&db, "SELECT 1_000"), vec![vec![integer(1000)]]);
    assert_eq!(rows(&db, "SELECT 1_000_000"), vec![vec![integer(1_000_000)]]);
    assert_eq!(rows(&db, "SELECT 1e1_0"), vec![vec![Value::Double(1e10)]]);
    let sql = "SELECT 1_0.5_0";
    assert_eq!(db.query(sql).unwrap().types(), &[LogicalType::Decimal { width: 4, scale: 2 }]);
    assert_eq!(rows(&db, sql), vec![vec![Value::Decimal { unscaled: 1050, width: 4, scale: 2 }]]);
}

/// A zero divisor is three different things, one per operator. Per #262.
///
/// `/` never raises, because the binder has already promoted both sides to DOUBLE and IEEE
/// arithmetic answers an infinity or a nan. `//` always raises, whatever it was given. `%` raises on
/// integers and on decimals and answers a nan on floats. All three were measured on the pinned
/// binary, and none of them is null, which is what this used to answer for all of them.
#[test]
fn dividing_by_zero_raises_on_two_of_the_three_operators() {
    let db = Database::new();
    let double = |value| vec![vec![Value::Double(value)]];
    assert_eq!(rows(&db, "SELECT 7 / 0"), double(f64::INFINITY));
    assert_eq!(rows(&db, "SELECT (-7) / 0"), double(f64::NEG_INFINITY));
    assert!(matches!(rows(&db, "SELECT 0 / 0")[0][0], Value::Double(answer) if answer.is_nan()));
    assert!(matches!(rows(&db, "SELECT 7.5::DOUBLE % 0.0::DOUBLE")[0][0],
            Value::Double(answer) if answer.is_nan()));
    // The vectorized loop, which reaches the zero at the third row rather than at the first.
    let run = rows(&db, "SELECT 10 / a FROM range(-1, 2) t(a)");
    assert_eq!(run[0], vec![Value::Double(-10.0)]);
    assert!(matches!(run[1][0], Value::Double(answer) if answer.is_infinite()));
    assert_eq!(run[2], vec![Value::Double(10.0)]);
}

/// The division by zero sentence, word for word against the pinned binary. Per #262.
///
/// It quotes the expression rather than the two values, and the expression it quotes is the bound
/// one: the casts the binder put in are in the text, a column is the name its table gives it, and a
/// literal is the value it was folded to.
#[test]
fn dividing_by_zero_says_what_duckdb_says() {
    let db = Database::new();
    db.create_table("z", vec![Field::new("a", LogicalType::Integer)]).unwrap();
    db.append("z", &[vec![Value::Integer(1)], vec![Value::Integer(-2)]]).unwrap();
    let advice = "Use TRY(...) to return NULL for this expression, or SET \
                  null_on_division_by_zero=true to return NULL for all divisions by zero.";
    let cases = [
        ("SELECT 7 // 0", "(7 // 0)"),
        ("SELECT 7 % 0", "(7 % 0)"),
        ("SELECT 7.50 % 0.00", "(7.50 % 0.00)"),
        ("SELECT 7.0::DOUBLE // 0.0::DOUBLE", "(7.0 // 0.0)"),
        // A column, so the constant folder leaves the row alone and the vectorized loop is what
        // raises. Everything below this line goes through that loop.
        ("SELECT a // 0 FROM z", "(a // 0)"),
        ("SELECT a % 0 FROM z", "(a % 0)"),
        ("SELECT a::DOUBLE // 0.0 FROM z", "(CAST(a AS DOUBLE) // 0.0)"),
        ("SELECT (a + 1) // 0 FROM z", "((a + 1) // 0)"),
        ("SELECT abs(a) % 0 FROM z", "(abs(a) % 0)"),
        // Unary minus brackets its operand where a binary operator does not, which is measured and
        // is what DuckDB does for every operator that is not binary.
        ("SELECT -a // 0 FROM z", "(-(a) // 0)"),
        ("SELECT 10 // (a - a) FROM z", "(10 // (a - a))"),
        ("SELECT 10.5 % (a - a) FROM z", "(10.5 % CAST((a - a) AS DECIMAL(11,1)))"),
    ];
    for (sql, quoted) in cases {
        let expected = format!("Division by zero in expression {quoted}. {advice}");
        assert_eq!(failure(&db, sql), expected, "{sql}");
    }
}

/// The sum of the two largest `DECIMAL(18,0)` values, which does not fit in a `DECIMAL(18,0)`.
///
/// It used to raise `Out of Range Error: Overflow in addition`, because the result kept the
/// operands' own type and two eighteen digit numbers add to nineteen digits. DuckDB answers
/// 1999999999999999998 typed `DECIMAL(19,0)` and now so does this. Per #243.
#[test]
fn adding_two_decimals_widens_the_answer_by_the_digit_the_carry_needs() {
    let db = Database::new();
    let sql = "SELECT 999999999999999999::DECIMAL(18,0) + 999999999999999999::DECIMAL(18,0) AS s";
    let result = db.query(sql).unwrap();
    assert_eq!(result.types(), &[LogicalType::Decimal { width: 19, scale: 0 }]);
    assert_eq!(
        rows(&db, sql),
        vec![vec![Value::Decimal { unscaled: 1_999_999_999_999_999_998, width: 19, scale: 0 }]]
    );
    // The mixed case, where nothing overflows and the type was one digit narrower than upstream.
    assert_eq!(
        db.query("SELECT 2.0 + 1::INTEGER").unwrap().types(),
        &[LogicalType::Decimal { width: 12, scale: 1 }]
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
fn a_join_that_runs_too_long_is_stopped_partway_through_its_own_loop() {
    // The timeout is checked between the chunks an operator produces, and a nested loop join
    // produces its first chunk only after the whole join is done. Fifty thousand left rows against
    // twenty thousand right ones is a billion comparisons and about a minute, all of it inside one
    // call, so without a check in the loop itself the clock below is read once, at the end.
    // A second rather than the fifty milliseconds the other limit tests use, because the limit is
    // on the database and not on the query, so the two statements below are run under it too. They
    // are a few milliseconds of work on an idle machine and they were over fifty on a busy one,
    // which failed the gate here on a setup line rather than on anything this test is about. A
    // second is two hundred times what the setup needs and a sixtieth of what the join needs, so it
    // still proves the only thing at issue, which is that the join is stopped inside its own loop.
    let db = Database::with_config(Config::new().with_query_timeout(Duration::from_secs(1)));
    db.execute("CREATE TABLE l AS SELECT i AS k FROM range(50000) t(i)").unwrap();
    db.execute("CREATE TABLE r AS SELECT i * 2 AS k FROM range(20000) t(i)").unwrap();
    let started = std::time::Instant::now();
    let error =
        db.query("SELECT count(*) FROM l JOIN r ON l.k = r.k").expect_err("that does not finish");
    assert_eq!(error.code().duckdb_name(), "Interrupt Error");
    // One left row's pass over the right side is what it may overshoot by, which is milliseconds.
    assert!(started.elapsed() < Duration::from_secs(10), "{:?}", started.elapsed());
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

/// A group key is charged what the table kept rather than what the buffer it was read into had
/// room for. Per #269.
#[test]
fn one_long_group_key_does_not_charge_every_short_one_for_its_length() {
    // Four megabytes, where the honest charge for these rows is a few hundred kilobytes and the
    // charge this is guarding against is two hundred copies of the long key, which is twenty
    // megabytes.
    let db = Database::with_config(Config::new().with_memory_limit(4 << 20));
    db.create_table("k", vec![Field::new("s", LogicalType::Varchar)]).unwrap();
    // The long one first, because the buffer being reused is what carries its length into every
    // key after it and a buffer never gives room back.
    let mut rows = vec![vec![Value::Varchar("x".repeat(100_000))]];
    rows.extend((0..200).map(|n| vec![Value::Varchar(format!("group {n}"))]));
    db.append("k", &rows).unwrap();
    let result = db.query("SELECT s, count(*) FROM k GROUP BY s").expect("the table fits");
    assert_eq!(result.len(), 201);
    // And the same query with the limit taken away agrees about the rows, so what is being tested
    // is the accounting rather than the grouping.
    let loose = Database::new();
    loose.create_table("k", vec![Field::new("s", LogicalType::Varchar)]).unwrap();
    loose.append("k", &rows).unwrap();
    assert_eq!(loose.query("SELECT s, count(*) FROM k GROUP BY s").unwrap().len(), 201);
}

/// A group by whose table does not fit the budget spills and answers anyway. Per #220.
///
/// This used to be the test for #272, which is the charge for the rows a group by builds out of its
/// table, and the way it asserted that charge was that a million groups did not fit in three
/// hundred megabytes. They do now, because a table that cannot grow any further stops growing and
/// the rows that would have gone into it go to a file instead. The charge is still there and is
/// still what decides when that happens, so the boundary this test sits on is the same one, and
/// what is on the far side of it has changed from an error to an answer.
#[test]
fn a_group_by_too_large_for_its_budget_spills_rather_than_stopping() {
    // A million groups against three hundred megabytes, where the table and the rows and the chunks
    // come to more than that between them.
    let db = Database::with_config(Config::new().with_memory_limit(300 << 20));
    let query = "SELECT range, count(*) FROM range(1000000) GROUP BY range";
    assert_eq!(db.query(query).expect("it spills rather than stopping").len(), 1_000_000);
    assert!(
        db.memory().peak() <= 300 << 20,
        "{} was held to get there, against a limit of {}",
        db.memory().peak(),
        300 << 20
    );
    // And a budget that does not hold the answer still refuses, quickly. Spilling moves the rows
    // out of the table and not out of the result, so there is a size of budget no number of passes
    // gets a query under, and the aggregate says so while its spill file is small rather than after
    // it has written the whole input out and read it back sixty four times.
    let tight = Database::with_config(Config::new().with_memory_limit(SMALL));
    let error = tight.query(query).err().unwrap_or_else(|| panic!("a megabyte holds none of this"));
    assert_eq!(error.code().duckdb_name(), "Out of Memory Error");
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
fn query_metrics_report_the_memory_budget_high_water_mark() {
    let db = Database::new();
    let result = db.query("SELECT range, count(*) FROM range(10000) GROUP BY range").unwrap();
    let measured = result.metrics().expect("a query carries execution metrics");
    assert!(measured.resource.peak_bytes > 0, "a buffering operator reserved memory");
    assert_eq!(measured.resource.peak_bytes, db.memory().peak());
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
fn every_name_the_pass_list_publishes_is_a_name_the_statement_accepts() {
    // The two have to agree or the corpus cannot turn the optimizer off, since the way it does
    // that is to join this list with commas and hand it to the statement. A pass added without a
    // name the setting knows would fail here rather than in another repository.
    let names = crate::optimizers();
    assert!(names.len() >= 6, "{names:?}");
    let db = Database::new();
    for name in &names {
        db.execute(&format!("SET disabled_optimizers = '{name}'")).expect(name);
    }
    let all = names.join(",");
    db.execute(&format!("SET disabled_optimizers = '{all}'")).expect("every pass off at once");
    // Read back in alphabetical order rather than in the order they run, because that is the order
    // the binary reads them back in and the setting is a compatibility surface.
    let mut sorted = names.clone();
    sorted.sort_unstable();
    assert_eq!(db.setting("disabled_optimizers").unwrap(), sorted.join(","));
    // And with all of them off the query still answers, out of the plan the binder produced.
    assert_eq!(db.value("SELECT 1 + 2").unwrap(), Value::Integer(3));
    let unoptimized = db.plan("SELECT 1 + 2").unwrap();
    assert!(unoptimized.contains("\"+\""), "{unoptimized}");
}

#[test]
fn a_pass_that_nobody_has_is_refused_by_the_statement_that_named_it() {
    let db = Database::new();
    let error = db.execute("SET disabled_optimizers = 'no_such_pass'").unwrap_err();
    assert_eq!(error.code().duckdb_name(), "Parser Error");
    assert_eq!(db.setting("disabled_optimizers").unwrap(), "", "a refused set changed nothing");
}

#[test]
fn a_pass_duckdb_has_and_rudb_has_not_built_is_taken_and_does_nothing() {
    // Forty five files in the upstream corpus run a SET disabled_optimizers and most of them name
    // a pass rudb has not written. Refusing those fails the SET, and a failed SET in a
    // sqllogictest file ends the file, so every record after it goes unasked over a pass whose
    // absence changes no answer.
    let db = Database::new();
    let folded = db.plan("SELECT 1 + 2").unwrap();
    for name in ["join_order", "build_side_probe_side", "statistics_propagation"] {
        db.execute(&format!("SET disabled_optimizers = '{name}'")).expect(name);
        assert_eq!(db.setting("disabled_optimizers").unwrap(), name);
        assert_eq!(db.plan("SELECT 1 + 2").unwrap(), folded, "{name} turned something off");
    }
}

#[test]
fn the_name_is_read_without_regard_to_case_because_two_corpus_files_shout_it() {
    let db = Database::new();
    db.execute("SET disabled_optimizers = 'LATE_MATERIALIZATION'").unwrap();
    assert_eq!(db.setting("disabled_optimizers").unwrap(), "late_materialization");
    db.execute("SET disabled_optimizers = 'Top_N'").unwrap();
    assert_eq!(db.setting("disabled_optimizers").unwrap(), "top_n");
}

#[test]
fn the_setting_reads_back_as_what_was_understood_rather_than_as_what_was_written() {
    // The binary tidies the list on the way in, so ' TOP_N , join_order , top_n ,' reads back as
    // join_order,top_n. A database that kept the text as written would answer current_setting
    // differently for every spelling but the tidy one.
    let db = Database::new();
    db.execute("SET disabled_optimizers = ' TOP_N , join_order , top_n ,'").unwrap();
    assert_eq!(db.setting("disabled_optimizers").unwrap(), "join_order,top_n");
}

#[test]
fn every_name_the_binary_accepts_is_a_name_the_statement_accepts() {
    // The list is written down rather than discovered, so the thing that can go wrong with it is a
    // typo, and a typo in it is a corpus file that fails on a name the binary is happy with.
    let db = Database::new();
    for name in rudb_opt::UPSTREAM {
        db.execute(&format!("SET disabled_optimizers = '{name}'")).expect(name);
        assert_eq!(db.setting("disabled_optimizers").unwrap(), name);
    }
    assert_eq!(rudb_opt::UPSTREAM.len(), 44, "the pinned binary lists forty four");
    // The table remains DuckDB's list. A local pass may have a local name, but adding one must not
    // make an introspection query claim that DuckDB has it too.
    assert!(!rudb_opt::UPSTREAM.contains(&"dependent_group_keys"));
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
fn a_seam_is_set_and_read_back_through_the_statement_everything_else_goes_through() {
    let db = Database::new();
    assert_eq!(db.setting("seam.hash.table").unwrap(), "default");

    // Quoted, because a seam name has dots in it and DuckDB's grammar has no dot in an identifier.
    db.execute("SET \"seam.hash.table\" = 'unchained'").unwrap();
    assert_eq!(db.setting("seam.hash.table").unwrap(), "unchained");
    assert_eq!(db.seams().pinned(crate::seam::SeamId::HashTable), Some("unchained"));

    // Underscores for the dots is the spelling that needs no quotes, and it is the same seam.
    db.execute("SET seam_hash_table = 'linear-chained'").unwrap();
    assert_eq!(db.setting("hash.table").unwrap(), "linear-chained");

    db.execute("RESET seam_hash_table").unwrap();
    assert_eq!(db.setting("seam.hash.table").unwrap(), "default");
    assert_eq!(db.seams().pinned(crate::seam::SeamId::HashTable), None);
}

#[test]
fn the_policy_is_a_seam_like_the_rest_and_a_mistyped_one_names_the_seams() {
    let db = Database::new();
    db.execute("SET seam_policy = 'reference'").unwrap();
    assert_eq!(db.seams().mode(), crate::seam::PolicyMode::Reference);
    assert_eq!(db.setting("seam.policy").unwrap(), "reference");

    let error = db.execute("SET \"seam.hash.tabel\" = 'unchained'").unwrap_err();
    assert_eq!(error.code().duckdb_name(), "Catalog Error");
    assert!(error.message().contains("rudb_strategies()"), "{error}");

    let error = db.execute("SET seam_policy = 'clever'").unwrap_err();
    assert!(error.message().contains("reference, default or adaptive-bandit"), "{error}");
}

#[test]
fn a_hint_pins_a_seam_for_one_query_and_leaves_the_session_alone() {
    let db = Database::new();
    let sql = "SELECT /*+ hash.table(unchained) */ 42";
    assert_eq!(db.value(sql).unwrap(), Value::Integer(42));

    let seams = db.seams_for(sql).unwrap();
    assert_eq!(seams.pinned(crate::seam::SeamId::HashTable), Some("unchained"));
    assert_eq!(db.seams().pinned(crate::seam::SeamId::HashTable), None, "the session is untouched");
}

#[test]
fn a_hint_naming_a_seam_nobody_has_fails_the_query_rather_than_being_ignored() {
    let db = Database::new();
    let error = db.query("SELECT /*+ hash.tabel(unchained) */ 42").unwrap_err();
    assert_eq!(error.code().duckdb_name(), "Catalog Error");
    assert!(error.message().contains("no seam called hash.tabel"), "{error}");

    // A comment without the plus is a comment, whatever is written in it.
    assert_eq!(db.value("SELECT /* hash.tabel(unchained) */ 42").unwrap(), Value::Integer(42));
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

#[test]
fn rudb_strategies_lists_every_seam_with_the_milestone_that_owes_it() {
    let db = database();
    let listed = rows(&db, "SELECT DISTINCT seam, milestone FROM rudb_strategies() ORDER BY seam");
    assert_eq!(listed.len(), 27, "the seam list is closed and this is its length");
    for row in &listed {
        let Value::Varchar(milestone) = &row[1] else { panic!("a milestone per seam") };
        assert!(milestone.starts_with('F'), "{milestone} is not a milestone");
    }
}

#[test]
fn a_seam_nobody_has_implemented_reads_back_as_planned_rather_than_missing() {
    let db = database();
    let listed = rows(
        &db,
        "SELECT count(*) FROM rudb_strategies() WHERE implementation IS NULL AND seam_description \
         IS NOT NULL",
    );
    assert_eq!(
        listed,
        vec![vec![Value::BigInt(26)]],
        "one seam has implementations and the rest say what milestone owes them"
    );
}

/// The first seam with implementations in the tree, as the table function shows it.
///
/// Three rows rather than one, the reference marked, and the description of what each one does,
/// which is what somebody deciding whether to sweep this seam reads before they do.
#[test]
fn the_compaction_seam_lists_its_three_implementations() {
    let db = database();
    let listed = rows(
        &db,
        "SELECT implementation, is_reference FROM rudb_strategies() WHERE seam = \
         'chunk.compaction'",
    );
    let names: Vec<&Value> = listed.iter().map(|row| &row[0]).collect();
    assert_eq!(
        names,
        vec![
            &Value::Varchar("never".to_owned()),
            &Value::Varchar("fixed-threshold".to_owned()),
            &Value::Varchar("learned-gain".to_owned()),
        ]
    );
    assert_eq!(listed[0][1], Value::Boolean(true), "the one that copies nothing is the reference");
}

#[test]
fn rudb_strategies_takes_no_arguments() {
    let db = database();
    assert!(
        failure(&db, "SELECT * FROM rudb_strategies(1)").contains("\"rudb_strategies\"()"),
        "a call with an argument is a binder error rather than an ignored argument"
    );
}

/// The operator of this kind, for a test that wants to look at one.
fn operator<'a>(metrics: &'a rudb_metrics::Document, kind: &str) -> &'a rudb_metrics::Operator {
    metrics
        .operators
        .iter()
        .find(|operator| operator.kind == kind)
        .unwrap_or_else(|| panic!("a {kind} in {:?}", metrics.operators))
}

#[test]
fn a_query_reports_what_every_operator_in_it_did() {
    let db = database();
    let result = db.query("SELECT x FROM t WHERE x > 1").unwrap();
    let metrics = result.metrics().expect("a query that ran has metrics");
    assert_eq!(metrics.query.sql, "SELECT x FROM t WHERE x > 1");
    let scan = operator(metrics, "Scan");
    let filter = operator(metrics, "Filter");
    assert_eq!(scan.detail.as_deref(), Some("t"), "a scan says what it read");
    assert_eq!(scan.rows_out, 4, "the table has four rows and the scan produced them");
    assert_eq!(filter.rows_in, 4, "what the scan produced is what the filter was handed");
    assert_eq!(filter.rows_out, 2, "two rows are over one");
    assert_eq!(filter.pipeline, scan.pipeline, "nothing here breaks a pipeline");
    assert!(metrics.timing.execute_ns > 0, "running it took longer than nothing");
    assert!(
        metrics.operators.iter().all(|operator| operator.reference_impl),
        "everything at tier 0 is the reference implementation and the document says so"
    );
    let filter = operator(metrics, "Filter");
    assert_eq!(filter.implementations.len(), 1, "a filter sits on one registered seam");
    assert_eq!(filter.implementations[0].seam, "chunk.compaction");
    assert_eq!(filter.implementations[0].name, "never");
}

#[test]
fn an_operator_that_was_pinned_off_the_reference_stops_being_marked_as_one() {
    // What the flag was supposed to do all along and could not, because it was set to true on
    // every operator whatever had run. The seam is pinned to something that is not the reference,
    // so the filter's row has to say so and every other row has to be unaffected.
    let db = database();
    db.execute("SET seam_chunk_compaction = 'learned-gain'").unwrap();
    let result = db.query("SELECT x FROM t WHERE x > 1").unwrap();
    let metrics = result.metrics().expect("a query that ran has metrics");
    let filter = operator(metrics, "Filter");
    assert!(!filter.reference_impl, "{:?}", filter.implementations);
    assert_eq!(filter.implementations[0].name, "learned-gain");
    assert!(operator(metrics, "Scan").reference_impl, "a scan sits on no registered seam");
}

#[test]
fn every_operator_has_its_own_id_and_a_parent_is_numbered_before_its_children() {
    let db = database();
    let result = db.query("SELECT count(*) FROM t WHERE x > 1").unwrap();
    let metrics = result.metrics().expect("a query that ran has metrics");
    let ids: Vec<u32> = metrics.operators.iter().map(|operator| operator.id).collect();
    assert_eq!(ids, (0..u32::try_from(ids.len()).unwrap()).collect::<Vec<_>>());
    let scan = operator(metrics, "Scan");
    let filter = operator(metrics, "Filter");
    assert!(filter.id < scan.id, "the filter is above the scan, so it is numbered first");
}

#[test]
fn a_sort_is_a_pipeline_that_the_one_above_it_waits_for() {
    let db = database();
    let result = db.query("SELECT x FROM t ORDER BY x").unwrap();
    let metrics = result.metrics().expect("a query that ran has metrics");
    let sort = operator(metrics, "Sort");
    assert_eq!(metrics.pipelines.len(), 2, "a sort breaks the pipeline in two");
    assert_eq!(metrics.pipelines[0].id, 0);
    assert_eq!(metrics.pipelines[0].depends_on, vec![1], "the root waits for the sort");
    assert_eq!(sort.pipeline, 1, "the sort ends the pipeline below");
    assert_eq!(sort.rows_in, 4, "every row went into it");
    assert!(metrics.pipelines[1].wall_ns > 0, "a pipeline's time is its operators' time");
}

#[test]
fn a_join_is_three_pipelines_in_the_order_they_have_to_run() {
    let db = database();
    let result = db.query("SELECT t.x FROM t JOIN t AS u ON t.x = u.x").unwrap();
    let metrics = result.metrics().expect("a query that ran has metrics");
    assert_eq!(metrics.pipelines.len(), 3, "one to gather the build side, one to probe, one above");
    let gather = operator(metrics, "Gather");
    let join = operator(metrics, "Join");
    assert_eq!(
        metrics.pipelines[0].depends_on,
        vec![join.pipeline],
        "the root waits for the probe"
    );
    assert_eq!(
        metrics.pipelines[usize::try_from(join.pipeline).unwrap()].depends_on,
        vec![gather.pipeline],
        "the probe waits for the side that is gathered first"
    );
    assert_eq!(gather.rows_in, 4, "the whole right side was gathered");
}

#[test]
fn a_statement_that_runs_no_plan_has_nothing_to_report() {
    let db = database();
    assert!(db.query("EXPLAIN SELECT 1").unwrap().metrics().is_none(), "explain runs nothing");
    assert!(db.execute("SET threads = 2").unwrap().metrics().is_none(), "a setting runs nothing");
}

#[test]
fn the_document_a_query_produces_is_the_json_a_harness_reads() {
    let db = database();
    let result = db.query("SELECT count(*) FROM t").unwrap();
    let written = result.metrics().expect("a query that ran has metrics").render();
    assert!(written.starts_with("{\n  \"schema\": 1,"), "{written}");
    assert!(written.contains("\"sql\": \"SELECT count(*) FROM t\""), "{written}");
    assert!(written.contains("\"kind\": \"Scan\""), "{written}");
    assert!(written.contains("\"reference_impl\": true"), "{written}");
}

#[test]
fn related_integer_sums_keep_null_and_empty_rules() {
    let db = database();
    let query = "SELECT sum(x), sum(x + 1) FROM \
                 (VALUES (1::SMALLINT), (NULL::SMALLINT), (3::SMALLINT)) AS v(x)";
    assert_eq!(rows(&db, query), vec![vec![Value::HugeInt(4), Value::HugeInt(6)]]);
    let empty = format!("{query} WHERE false");
    assert_eq!(rows(&db, &empty), vec![vec![Value::Null, Value::Null]]);
}

/// A database that will run a query on `threads` of them, whatever the machine has.
///
/// Pinned rather than left at the default, because the default is however many cores the test
/// runner happens to have and a parallel test that runs on one core proves nothing. Eight is enough
/// to shuffle the order of anything that depends on it and small enough not to matter on a busy
/// machine.
fn threaded(threads: usize) -> Database {
    Database::with_config(Config::new().with_threads(threads).unwrap())
}

#[test]
fn a_query_on_eight_threads_answers_what_it_answers_on_one() {
    let sql = "SELECT count(*), sum(range), min(range), max(range) \
               FROM range(1000000) WHERE range % 7 = 0";
    assert_eq!(rows(&threaded(8), sql), rows(&threaded(1), sql));
}

#[test]
fn a_group_by_on_eight_threads_finds_every_group_exactly_once() {
    let sql = "SELECT range % 1000 AS k, count(*), sum(range) FROM range(1000000) GROUP BY k";
    let mut many = rows(&threaded(8), sql);
    let mut one = rows(&threaded(1), sql);
    many.sort_by_key(|row| format!("{:?}", row[0]));
    one.sort_by_key(|row| format!("{:?}", row[0]));
    assert_eq!(many.len(), 1000);
    assert_eq!(many, one);
}

/// The nine ClickBench queries that did not get faster when pipelines learned to run wide all have
/// one of these in them, because the aggregate refused a second instance and the refusal dropped the
/// scan under it back to one thread. See #509.
#[test]
fn a_count_distinct_on_eight_threads_counts_each_value_once() {
    let sql = "SELECT count(DISTINCT range % 977) FROM range(200000)";
    assert_eq!(rows(&threaded(8), sql), [vec![Value::BigInt(977)]]);
    assert_eq!(rows(&threaded(8), sql), rows(&threaded(1), sql));
}

/// Distinct sets merge by offering their values to the kept set, so an aggregate other than a count
/// gets the same answer rather than only the counting one being right.
#[test]
fn every_distinct_aggregate_on_eight_threads_answers_what_it_answers_on_one() {
    let sql = "SELECT sum(DISTINCT range % 1009), min(DISTINCT range % 1009), \
               max(DISTINCT range % 1009), count(DISTINCT range % 1009) FROM range(300000)";
    let total: i64 = (0..1009i64).sum();
    assert_eq!(
        rows(&threaded(8), sql),
        [vec![
            Value::HugeInt(i128::from(total)),
            Value::BigInt(0),
            Value::BigInt(1008),
            Value::BigInt(1009)
        ]]
    );
    assert_eq!(rows(&threaded(8), sql), rows(&threaded(1), sql));
}

/// A distinct count inside a group by, where the sets being merged belong to groups that may or may
/// not be in the table they are merged into.
#[test]
fn a_grouped_count_distinct_on_eight_threads_finds_every_group_and_every_value() {
    let sql = "SELECT range % 13 AS k, count(DISTINCT range % 91), count(*) \
               FROM range(400000) GROUP BY k";
    let mut many = rows(&threaded(8), sql);
    let mut one = rows(&threaded(1), sql);
    many.sort_by_key(|row| format!("{:?}", row[0]));
    one.sort_by_key(|row| format!("{:?}", row[0]));
    assert_eq!(many.len(), 13);
    for row in &many {
        assert_eq!(row[1], Value::BigInt(7), "91 over 13 is seven values in each group");
    }
    assert_eq!(many, one);
}

/// A distinct over a string, which takes the other set in the operator, since a BIGINT argument gets
/// a set of integers and everything else gets a set of rows.
#[test]
fn a_count_distinct_over_strings_on_eight_threads_counts_each_string_once() {
    let sql = "SELECT count(DISTINCT 'tag' || (range % 641)) FROM range(200000)";
    assert_eq!(rows(&threaded(8), sql), [vec![Value::BigInt(641)]]);
    assert_eq!(rows(&threaded(8), sql), rows(&threaded(1), sql));
}

/// Eight instances of a group by, each row landing in the partition its key hashes to.
///
/// Every instance sees part of every group, so each group is reached from eight threads and has to
/// come out holding all eight contributions exactly once. What the answer checks is that the split
/// loses nothing and doubles nothing: the number of groups, the counts and the sums all come out as
/// if one thread had done it.
#[test]
fn a_high_cardinality_group_by_on_eight_threads_merges_to_the_same_answer() {
    let sql = "SELECT range % 50000 AS g, count(*), sum(range) FROM range(400000) GROUP BY g";
    let many = rows(&threaded(8), sql);
    assert_eq!(many.len(), 50_000);
    let mut sorted = many.clone();
    sorted.sort_by_key(|row| match row[0] {
        Value::BigInt(key) => key,
        _ => unreachable!("the key is a BIGINT"),
    });
    assert_eq!(sorted[0], vec![Value::BigInt(0), Value::BigInt(8), Value::HugeInt(1_400_000)]);
    let mut one = rows(&threaded(1), sql);
    one.sort_by_key(|row| match row[0] {
        Value::BigInt(key) => key,
        _ => unreachable!("the key is a BIGINT"),
    });
    assert_eq!(sorted, one);
}

/// A distinct set split across the partitions, where the sets are the expensive half of a group.
///
/// Enough groups to be spread over every partition, and a `DISTINCT` inside each one so that what a
/// partition accumulates is a set rather than just a running total. Each group holds three values
/// and every instance offers it some of all three.
#[test]
fn a_grouped_count_distinct_over_many_groups_on_eight_threads_agrees_with_one_thread() {
    let sql = "SELECT range % 30000 AS k, count(DISTINCT range % 90000), count(*) \
               FROM range(900000) GROUP BY k";
    let many = rows(&threaded(8), sql);
    assert_eq!(many.len(), 30_000);
    for row in &many {
        assert_eq!(row[1], Value::BigInt(3), "90000 over 30000 is three values in each group");
        assert_eq!(row[2], Value::BigInt(30), "900000 over 30000 is thirty rows in each group");
    }
    let mut sorted = many;
    let mut one = rows(&threaded(1), sql);
    sorted.sort_by_key(|row| format!("{:?}", row[0]));
    one.sort_by_key(|row| format!("{:?}", row[0]));
    assert_eq!(sorted, one);
}

/// A group by on eight threads whose budget makes it spill on the way to partitioning.
///
/// The awkward case, which is an instance whose own table runs out of room before it has handed its
/// groups to the partitions. Its spill file covers every partition, so it cannot be scattered, and
/// what happens instead is that the groups still in the table are scattered and the file is read
/// back and spread row by row. Both halves have to land, and each row has to land once.
///
/// Three hundred megabytes against a million groups is the same boundary
/// `a_group_by_too_large_for_its_budget_spills_rather_than_stopping` sits on, with eight threads
/// under it so that the instances race for the budget rather than taking it in turn.
#[test]
fn a_group_by_that_spills_on_eight_threads_answers_what_it_answers_on_one() {
    let sql = "SELECT range % 900000 AS k, count(*), sum(range) FROM range(1800000) GROUP BY k";
    let db = Database::with_config(
        Config::new().with_memory_limit(300 << 20).with_threads(8).expect("eight threads"),
    );
    let answer = db.query(sql).expect("it spills rather than stopping");
    let mut many: Vec<Vec<Value>> = answer.rows().collect();
    assert_eq!(many.len(), 900_000);
    let mut one = rows(&threaded(1), sql);
    many.sort_by_key(|row| first_key(row));
    one.sort_by_key(|row| first_key(row));
    assert_eq!(many, one, "every row landed once whether it went through a spill file or not");
}

/// The same over a string key, since a partition gathers its keys out of the chunk before folding.
#[test]
fn a_group_by_a_string_with_many_groups_on_eight_threads_agrees_with_one_thread() {
    let sql = "SELECT 'k' || (range % 25000) AS k, count(*), min(range), max(range) \
               FROM range(500000) GROUP BY k";
    let many = rows(&threaded(8), sql);
    assert_eq!(many.len(), 25_000);
    let mut sorted = many;
    let mut one = rows(&threaded(1), sql);
    sorted.sort_by_key(|row| format!("{:?}", row[0]));
    one.sort_by_key(|row| format!("{:?}", row[0]));
    assert_eq!(sorted, one);
}

#[test]
fn a_query_with_no_order_by_keeps_the_source_order_on_eight_threads() {
    let rows = rows(&threaded(8), "SELECT range FROM range(200000) WHERE range % 3 = 0");
    let read: Vec<Value> = rows.into_iter().map(|row| row[0].clone()).collect();
    let expected: Vec<Value> = (0..200_000i64).step_by(3).map(Value::BigInt).collect();
    assert_eq!(read, expected, "the scan was cut into morsels and put back together in order");
}

#[test]
fn an_order_by_on_eight_threads_is_still_in_order() {
    let rows = rows(
        &threaded(8),
        "SELECT range FROM range(100000) WHERE range % 1000 = 0 ORDER BY range DESC LIMIT 5",
    );
    assert_eq!(
        rows,
        vec![
            vec![Value::BigInt(99000)],
            vec![Value::BigInt(98000)],
            vec![Value::BigInt(97000)],
            vec![Value::BigInt(96000)],
            vec![Value::BigInt(95000)],
        ]
    );
}

#[test]
fn a_query_on_several_threads_says_so_in_its_metrics() {
    let db = threaded(8);
    let result =
        db.query("SELECT count(*), sum(range) FROM range(2000000) WHERE range % 7 = 0").unwrap();
    let metrics = result.metrics().unwrap();
    assert_eq!(metrics.settings.threads, 8, "the document says what the query was allowed");
    let widest = metrics.pipelines.iter().map(|pipeline| pipeline.instances).max().unwrap();
    assert_eq!(widest, 8, "and the scan says it used all of them");
    if rudb_metrics::thread_cpu_ns().is_some() {
        assert!(metrics.resource.cpu_ns > 0);
    }
}

#[test]
fn setting_threads_changes_what_the_next_query_may_use() {
    let db = threaded(8);
    db.execute("SET threads = 1").unwrap();
    assert_eq!(db.config().threads(), 1);
    let metrics = db.query("SELECT count(*) FROM range(1000000)").unwrap();
    let metrics = metrics.metrics().unwrap();
    assert_eq!(metrics.settings.threads, 1);
    let widest = metrics.pipelines.iter().map(|pipeline| pipeline.instances).max().unwrap();
    assert_eq!(widest, 1, "a setting nothing obeys is not a setting");
}

/// A group by whose instances spill before they partition, so the spill files are handed over too.
///
/// The awkward case. An instance that runs out of budget while it still holds its groups to itself
/// writes the rows it had no room for to a file, and that file covers every partition rather than
/// one of them. So when the instance does partition, the groups it still holds are scattered and
/// the file is read back and spread row by row. Both halves have to land, and each row exactly once.
///
/// Ten thousand groups is under the threshold, so what tips this instance into partitioning is the
/// budget rather than the group count, which is the only way to reach the case. The distinct set is
/// what fills the budget at so few groups: fifty values per group is fifty allocations per group
/// where a plain count is one counter. Twenty four megabytes is in the middle of the band that
/// crowds without running out, which on the machine this was written on runs from twelve to thirty
/// two. A budget outside the band still gives the right answer, it just covers less.
#[test]
fn a_group_by_that_spills_before_it_partitions_lands_the_file_and_the_table() {
    let sql = "SELECT range % 10000 AS k, count(DISTINCT range), count(*), sum(range) \
               FROM range(500000) GROUP BY k";
    let db = Database::with_config(
        Config::new().with_memory_limit(24 << 20).with_threads(8).expect("eight threads"),
    );
    let answer = db.query(sql).expect("it spills rather than stopping");
    let mut many: Vec<Vec<Value>> = answer.rows().collect();
    assert_eq!(many.len(), 10_000);
    let mut one = rows(&threaded(1), sql);
    many.sort_by_key(|row| first_key(row));
    one.sort_by_key(|row| first_key(row));
    assert_eq!(many, one, "every row landed once whether it came back from a file or not");
}

/// A table built from a query keeps whatever form the query produced, and a caller still gets flat
/// columns because a result set is the thing that flattens rather than the thing that stores.
///
/// The one that matters is the first assertion. Draining a query happens on one thread, so a copy
/// made there is a copy made while the rest of the pool has nothing to do, and on a wide load it is
/// most of what the statement costs. A dictionary column is the case where the copy is largest,
/// because flattening it writes one value per row out of a body that held one per distinct value.
#[test]
fn a_table_built_from_a_query_keeps_the_dictionary_the_query_produced() {
    let db = Database::new();
    db.execute("CREATE TABLE src AS SELECT range % 4 AS k FROM range(4096)").expect("src builds");
    // A filter over a small distinct set is what leaves a dictionary behind, which is the same
    // thing a parquet scan of a dictionary encoded column hands up.
    db.execute("CREATE TABLE kept AS SELECT k FROM src WHERE k < 3").expect("kept builds");
    let forms = db.with_catalog(|catalog| {
        let name = rudb_catalog::QualifiedName::new("memory", "main", "kept");
        let table = catalog.table(&name).expect("the table is there");
        (0..table.rows().chunk_count())
            .filter_map(|at| table.rows().chunk(at))
            .map(|chunk| chunk.column(0).expect("one column").form())
            .collect::<Vec<_>>()
    });
    assert!(!forms.is_empty(), "the table has chunks");
    assert!(
        forms.iter().any(|&form| form != rudb_vector::Form::Flat),
        "every chunk was flattened on the way in, so the drain still pays for a copy nobody wants"
    );
    let answer = db.query("SELECT count(*), sum(k) FROM kept").expect("it reads back");
    let rows: Vec<Vec<Value>> = answer.rows().collect();
    assert_eq!(rows, vec![vec![Value::BigInt(3072), Value::HugeInt(3072)]]);
}

#[test]
fn a_file_backed_insert_streams_into_a_snapshot_that_a_new_process_can_read() {
    let path = std::env::temp_dir().join(format!(
        "rudb-native-checkpoint-{}-{}.rdb",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("the clock advances")
            .as_nanos()
    ));
    let database = Database::open(path.to_str().expect("a UTF-8 temporary path"))
        .expect("the new native database opens");
    database.execute("CREATE TABLE hits (id INTEGER, name VARCHAR)").expect("the table is made");
    database
        .execute("INSERT INTO hits VALUES (1, 'one'), (2, NULL), (3, 'three')")
        .expect("the rows are inserted");
    assert!(path.exists(), "the insert publishes its snapshot");
    assert!(database.with_catalog(|catalog| {
        let name = rudb_catalog::QualifiedName::new("memory", "main", "hits");
        catalog.table(&name).expect("the table is there").rows().is_native()
    }));
    database.execute("CHECKPOINT").expect("checkpoint sees an already committed snapshot");
    drop(database);

    let reopened = Database::open(path.to_str().expect("a UTF-8 temporary path"))
        .expect("the committed native database reopens");
    assert_eq!(
        rows(&reopened, "SELECT count(*), sum(id), min(name) FROM hits"),
        vec![vec![Value::BigInt(3), Value::HugeInt(6), Value::Varchar("one".into())]]
    );
    std::fs::remove_file(path).expect("the temporary native database is removed");
}
