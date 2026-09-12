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
    // Upstream quotes the four that are keywords and that quoting is #251.
    assert_eq!(
        db.query("SELECT substring(s FROM 2 FOR 3) FROM t").unwrap().names(),
        &["substring(s, 2, 3)".to_string()]
    );
    assert_eq!(
        db.query("SELECT position('c' IN s) FROM t").unwrap().names(),
        &["position(s, 'c')".to_string()]
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
    // Upstream quotes this one, because NULLIF is a keyword and the name it prints is the macro's.
    // The quoting is #251 and the call is the same call.
    assert_eq!(
        db.query("SELECT NULLIF(x, 1) FROM t").unwrap().names(),
        &["nullif(x, 1)".to_string()]
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

/// One index into a list, which is as far as the list side of this gets today. Per #278.
///
/// A slice of a list answers a list, and there is no LIST vector yet, so `[1, 2, 3][1:2]` stops
/// with the same message `SELECT [1, 2, 3]` stops with. An index answers an element, and an element
/// of a list of numbers is a number, so these run all the way through.
#[test]
fn a_bracket_on_a_list_picks_one_element_out_of_it() {
    let db = database();
    assert_eq!(rows(&db, "SELECT [1,2,3][2]"), vec![vec![integer(2)]]);
    assert_eq!(rows(&db, "SELECT [1,2,3][-1]"), vec![vec![integer(3)]]);
    // Off either end of a list is a null, which is where the list and the string disagree.
    assert_eq!(rows(&db, "SELECT [1,2,3][0]"), vec![vec![Value::Null]]);
    assert_eq!(rows(&db, "SELECT [1,2,3][4]"), vec![vec![Value::Null]]);
    assert_eq!(rows(&db, "SELECT list_extract([1,2,3], 2)"), vec![vec![integer(2)]]);
    let error = db.query("SELECT [1,2,3][1:2]").unwrap_err();
    assert_eq!(error.code().duckdb_name(), "Not implemented Error");
    assert_eq!(error.message(), db.query("SELECT [1,2,3]").unwrap_err().message());
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
    let db = Database::with_config(Config::new().with_query_timeout(Duration::from_millis(50)));
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
